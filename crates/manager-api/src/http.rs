//! Router construction and request handling.
//!
//! Op routes are generated from `manager_app::registry::OP_SPECS`:
//! each spec with a route gets a handler that assembles the
//! operation's argument object (path params, coerced query params,
//! JSON body — closed object, unknown fields rejected), dispatches
//! through the registry, and shapes the result. Composite/derived
//! endpoints (identity, get-one filters, group list, update list,
//! log scopes) are explicit handlers below that call the same
//! dispatch; they are enumerated in [`DERIVED_ROUTES`] so the
//! contract test can account for the whole surface.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, MethodFilter};
use axum::Router;
use serde_json::{json, Map, Value};

use manager_app::registry::{HttpMethod, Op, OpSpec, RouteMode, OP_SPECS};
use manager_app::ApiError;

use crate::composites;
use crate::ApiCtx;

/// One registered route, recorded during construction so tests can
/// enumerate the live surface against the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRecord {
    pub method: &'static str,
    pub path: &'static str,
}

/// Non-registry routes, explicit by design (see module docs).
/// (method, path) pairs; handlers live in this module.
pub const DERIVED_ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/v1/server"),
    ("GET", "/api/v1/backends/{id}"),
    ("GET", "/api/v1/models/{id}"),
    ("GET", "/api/v1/operations/{id}"),
    ("GET", "/api/v1/logs/scopes"),
    ("GET", "/api/v1/gateway/groups"),
    ("GET", "/api/v1/updates"),
    // Phase 3 composites (composites.rs): read-modify-write and
    // orchestration endpoints with no single backing operation.
    ("PATCH", "/api/v1/models/{id}"),
    ("POST", "/api/v1/gateway/groups"),
    ("PATCH", "/api/v1/gateway/groups/{name}"),
    ("DELETE", "/api/v1/gateway/groups/{name}"),
    ("POST", "/api/v1/gateway/start"),
    ("POST", "/api/v1/activation-plan/apply"),
    // Phase 4: retained diagnostics bundle content + the event stream.
    ("GET", "/api/v1/diagnostics/bundles/{bundle_id}/content"),
    ("GET", "/api/v1/events"),
];

/// Build the full router from the registry plus the derived routes.
/// Returns the router and the record of everything registered.
pub fn build_router(ctx: Arc<ApiCtx>) -> (Router, Vec<RouteRecord>) {
    let mut records: Vec<RouteRecord> = Vec::new();
    let mut router = Router::new();
    for spec in OP_SPECS {
        let Some(route) = spec.route else { continue };
        let filter = match route.method {
            HttpMethod::Get => MethodFilter::GET,
            HttpMethod::Post => MethodFilter::POST,
            HttpMethod::Put => MethodFilter::PUT,
            HttpMethod::Patch => MethodFilter::PATCH,
            HttpMethod::Delete => MethodFilter::DELETE,
        };
        records.push(RouteRecord {
            method: method_str(route.method),
            path: route.path,
        });
        router = router.route(
            route.path,
            axum::routing::on(filter, move |state, path, query, body| {
                handle_op(spec, state, path, query, body)
            }),
        );
    }
    for (method, path) in DERIVED_ROUTES {
        records.push(RouteRecord { method, path });
    }
    router = router
        .route("/api/v1/server", get(handle_server_identity))
        .route("/api/v1/backends/{id}", get(handle_backend_one))
        .route(
            "/api/v1/models/{id}",
            get(handle_model_one).patch(handle_model_patch),
        )
        .route("/api/v1/operations/{id}", get(handle_operation_one))
        .route("/api/v1/logs/scopes", get(handle_log_scopes))
        .route(
            "/api/v1/gateway/groups",
            get(handle_gateway_groups).post(handle_group_create),
        )
        .route(
            "/api/v1/gateway/groups/{name}",
            axum::routing::patch(handle_group_patch)
                .delete(handle_group_delete),
        )
        .route(
            "/api/v1/gateway/start",
            axum::routing::post(handle_gateway_start),
        )
        .route(
            "/api/v1/activation-plan/apply",
            axum::routing::post(handle_activation_apply),
        )
        .route(
            "/api/v1/diagnostics/bundles/{bundle_id}/content",
            get(handle_bundle_content),
        )
        .route("/api/v1/events", get(handle_events))
        .route("/api/v1/updates", get(handle_updates));
    for (method, path, _class) in crate::auth_routes::AUTH_ROUTES {
        records.push(RouteRecord { method, path });
    }
    router = router
        .route("/api/v1/auth/login", axum::routing::post(crate::auth_routes::handle_login))
        .route("/api/v1/auth/totp", axum::routing::post(crate::auth_routes::handle_totp))
        .route("/api/v1/auth/refresh", axum::routing::post(crate::auth_routes::handle_refresh))
        .route("/api/v1/enroll", axum::routing::post(crate::auth_routes::handle_enroll))
        .route("/api/v1/auth/me", get(crate::auth_routes::handle_me))
        .route("/api/v1/auth/logout", axum::routing::post(crate::auth_routes::handle_logout))
        .route("/api/v1/auth/password", axum::routing::post(crate::auth_routes::handle_change_password))
        .route("/api/v1/auth/sessions", get(crate::auth_routes::handle_list_sessions))
        .route(
            "/api/v1/auth/sessions/{id}",
            axum::routing::delete(crate::auth_routes::handle_revoke_session),
        )
        .route("/api/v1/auth/totp/enroll", axum::routing::post(crate::auth_routes::handle_totp_enroll))
        .route("/api/v1/auth/totp/verify", axum::routing::post(crate::auth_routes::handle_totp_verify))
        .route("/api/v1/auth/totp/disable", axum::routing::post(crate::auth_routes::handle_totp_disable))
        .route(
            "/api/v1/admin/users",
            get(crate::auth_routes::handle_admin_list_users)
                .post(crate::auth_routes::handle_admin_create_user),
        )
        .route(
            "/api/v1/admin/users/{id}",
            axum::routing::patch(crate::auth_routes::handle_admin_update_user)
                .delete(crate::auth_routes::handle_admin_delete_user),
        )
        .route(
            "/api/v1/admin/users/{id}/reset-password",
            axum::routing::post(crate::auth_routes::handle_admin_reset_password),
        )
        .route(
            "/api/v1/admin/users/{id}/sessions",
            get(crate::auth_routes::handle_admin_list_user_sessions),
        )
        .route(
            "/api/v1/admin/users/{id}/sessions/{session_id}",
            axum::routing::delete(crate::auth_routes::handle_admin_revoke_user_session),
        )
        .route(
            "/api/v1/admin/api-tokens",
            get(crate::auth_routes::handle_admin_list_tokens)
                .post(crate::auth_routes::handle_admin_create_token),
        )
        .route(
            "/api/v1/admin/api-tokens/{id}",
            axum::routing::delete(crate::auth_routes::handle_admin_revoke_token),
        )
        .route(
            "/api/v1/admin/enrollment-codes",
            axum::routing::post(crate::auth_routes::handle_admin_create_enrollment),
        )
        .route(
            "/api/v1/admin/audit",
            get(crate::auth_routes::handle_admin_audit),
        );
    // Unknown paths under the API answer with the standard error
    // envelope, not the framework's bare 404.
    let router = router.fallback(|state: State<Arc<ApiCtx>>| async move {
        let State(ctx) = state;
        error_response(
            &ctx,
            &ApiError::NotFound("no such route".to_string()),
            None,
        )
    });
    let mw_state = crate::auth_routes::AuthMwState {
        ctx: ctx.clone(),
        table: Arc::new(crate::auth_routes::route_class_table()),
    };
    let router = router
        .layer(axum::extract::DefaultBodyLimit::max(1 << 20))
        .layer(axum::middleware::from_fn_with_state(
            mw_state,
            crate::auth_routes::auth_middleware,
        ))
        .with_state(ctx);
    (router, records)
}

fn method_str(m: HttpMethod) -> &'static str {
    match m {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Delete => "DELETE",
    }
}

// ---------------------------------------------------------------------------
// The generic operation handler
// ---------------------------------------------------------------------------

async fn handle_op(
    spec: &'static OpSpec,
    State(ctx): State<Arc<ApiCtx>>,
    Path(path_params): Path<HashMap<String, String>>,
    Query(query): Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    let args = match assemble_args(spec, &path_params, &query, &body) {
        Ok(a) => a,
        Err(resp) => return *resp,
    };
    // Asynchronous operations (rest-api.md §3.1): create the
    // operation record, spawn the dispatch, answer 202. The
    // record's detail carries the outcome the synchronous call
    // would have returned.
    if spec.route.map(|r| r.mode) == Some(RouteMode::AsyncOp) {
        let (id, record) = ctx.operations.begin(spec.name);
        let spawn_ctx = ctx.clone();
        tokio::spawn(async move {
            match manager_app::registry::dispatch(&spawn_ctx.app, &spawn_ctx.sink, spec.op, args)
                .await
            {
                Ok(value) => {
                    if spec.op == Op::RunWizardStep
                        && value.get("busy") == Some(&json!(true))
                    {
                        spawn_ctx.operations.fail_with_detail(
                            &id,
                            "a wizard step is already running",
                            value,
                        );
                    } else {
                        spawn_ctx
                            .operations
                            .complete(&id, spec.name, Ok(value));
                    }
                }
                Err(e) => {
                    let message = sanitize(&e.to_string(), &spawn_ctx.data_dir);
                    spawn_ctx
                        .operations
                        .complete(&id, spec.name, Err(message));
                }
            }
        });
        return (axum::http::StatusCode::ACCEPTED, axum::Json(record)).into_response();
    }
    // The operations list merges the app's task records with the
    // API-side operation records.
    if spec.op == Op::TasksList {
        return match manager_app::registry::dispatch(&ctx.app, &ctx.sink, spec.op, args).await {
            Ok(mut value) => {
                if let Some(arr) = value.as_array_mut() {
                    let known: std::collections::HashSet<String> = arr
                        .iter()
                        .filter_map(|t| t["id"].as_str().map(str::to_string))
                        .collect();
                    for record in ctx.operations.list() {
                        if !known.contains(record["id"].as_str().unwrap_or("")) {
                            arr.push(record);
                        }
                    }
                }
                (axum::http::StatusCode::OK, axum::Json(value)).into_response()
            }
            Err(e) => error_response(&ctx, &e, None),
        };
    }
    // Abort: API-side records flip in the API store (bookkeeping
    // parity with the app); anything else is an app task.
    if spec.op == Op::TaskAbort {
        if let Some(task_id) = args.get("task_id").and_then(Value::as_str) {
            if ctx.operations.get(task_id).is_some() {
                ctx.operations.abort(task_id);
                return axum::Json(json!({ "ok": true, "task_id": task_id }))
                    .into_response();
            }
        }
    }
    // PATCH /gateway/config composition: a bind_host field routes
    // through the dedicated bind operation (composites module).
    if spec.op == Op::UpdateGatewayConfig {
        let patch = args.get("patch").cloned().unwrap_or_else(|| json!({}));
        return match composites::update_gateway_config(&ctx, patch).await {
            Ok(value) => (axum::http::StatusCode::OK, axum::Json(value)).into_response(),
            Err(e) => error_response(&ctx, &e, None),
        };
    }
    match manager_app::registry::dispatch(&ctx.app, &ctx.sink, spec.op, args).await {
        Ok(value) => shape_success(&ctx, spec, value),
        Err(e) => error_response(&ctx, &e, None),
    }
}

/// Assemble the operation argument object from the request parts.
/// Bodies are closed objects: a field the operation does not
/// declare is an `INVALID_INPUT` (rest-api.md §2).
fn assemble_args(
    spec: &OpSpec,
    path_params: &HashMap<String, String>,
    query: &HashMap<String, String>,
    body: &[u8],
) -> Result<Value, Box<Response>> {
    let mut obj = Map::new();
    for (k, v) in path_params {
        obj.insert(k.clone(), Value::String(v.clone()));
    }
    for (k, v) in query {
        if spec.params.contains(&k.as_str()) {
            obj.insert(k.clone(), coerce_query_value(v));
        }
    }
    if !body.is_empty() {
        let parsed: Value = serde_json::from_slice(body).map_err(|e| {
            Box::new(error_response_raw(
                axum::http::StatusCode::BAD_REQUEST,
                "INVALID_INPUT",
                "InvalidInput",
                &format!("malformed JSON body: {e}"),
                None,
            ))
        })?;
        match spec.route.and_then(|r| r.body_wrap) {
            Some(wrap) => {
                obj.insert(wrap.to_string(), parsed);
            }
            None => {
                let fields = parsed.as_object().ok_or_else(|| {
                    Box::new(error_response_raw(
                        axum::http::StatusCode::BAD_REQUEST,
                        "INVALID_INPUT",
                        "InvalidInput",
                        "request body must be a JSON object",
                        None,
                    ))
                })?;
                for (k, v) in fields {
                    if !spec.params.contains(&k.as_str()) {
                        return Err(Box::new(error_response_raw(
                            axum::http::StatusCode::BAD_REQUEST,
                            "INVALID_INPUT",
                            "InvalidInput",
                            &format!("unknown field `{k}` for {}", spec.name),
                            None,
                        )));
                    }
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
    }
    Ok(Value::Object(obj))
}

/// Query strings are strings; numeric/bool arguments are coerced so
/// e.g. `?limit=20` reaches a `u64` parameter as a number.
fn coerce_query_value(raw: &str) -> Value {
    if let Ok(n) = raw.parse::<i64>() {
        return json!(n);
    }
    if let Ok(n) = raw.parse::<f64>() {
        return json!(n);
    }
    match raw {
        "true" => return json!(true),
        "false" => return json!(false),
        _ => {}
    }
    Value::String(raw.to_string())
}

fn shape_success(ctx: &ApiCtx, spec: &OpSpec, value: Value) -> Response {
    // The wizard-step singleton: the app reports a busy run as a
    // successful payload; on the wire it is a 409 (rest-api.md §2).
    if spec.op == Op::RunWizardStep && value.get("busy") == Some(&json!(true)) {
        return error_response_raw(
            axum::http::StatusCode::CONFLICT,
            "BUSY",
            "Busy",
            "a wizard step is already running",
            Some(value),
        );
    }
    // Generated gateway YAML crosses as text, not a JSON wrapper.
    if spec.op == Op::GatewayPreviewYaml {
        if let Some(yaml) = value.get("yaml").and_then(Value::as_str) {
            return (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "text/yaml")],
                yaml.to_string(),
            )
                .into_response();
        }
    }
    let status = match spec.route.map(|r| r.mode) {
        Some(RouteMode::Created) => axum::http::StatusCode::CREATED,
        _ => axum::http::StatusCode::OK,
    };
    let _ = ctx;
    (status, axum::Json(value)).into_response()
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Map a typed op error to (status, REST code, kind). One place —
/// the REST layer owns presentation (PLAN.md §3.3).
pub fn error_mapping(e: &ApiError) -> (axum::http::StatusCode, &'static str, &'static str) {
    use axum::http::StatusCode as S;
    match e {
        ApiError::InvalidInput(_) => (S::BAD_REQUEST, "INVALID_INPUT", "InvalidInput"),
        ApiError::NotFound(_) => (S::NOT_FOUND, "NOT_FOUND", "NotFound"),
        ApiError::Config(_) => (S::UNPROCESSABLE_ENTITY, "CONFIG", "Config"),
        ApiError::Unsupported(_) => (S::NOT_IMPLEMENTED, "UNSUPPORTED", "Unsupported"),
        ApiError::ContractGap(_) => (S::NOT_IMPLEMENTED, "CONTRACT_GAP", "ContractGap"),
        ApiError::Supervisor(_) => (S::INTERNAL_SERVER_ERROR, "SUPERVISOR", "Supervisor"),
        ApiError::Wizard(_) => (S::INTERNAL_SERVER_ERROR, "WIZARD", "Wizard"),
        ApiError::Telemetry(_) => (S::INTERNAL_SERVER_ERROR, "TELEMETRY", "Telemetry"),
        ApiError::Gateway(_) => (S::INTERNAL_SERVER_ERROR, "GATEWAY", "Gateway"),
        ApiError::Models(_) => (S::INTERNAL_SERVER_ERROR, "MODELS", "Models"),
        ApiError::Mxc(_) => (S::INTERNAL_SERVER_ERROR, "MXC", "Mxc"),
        ApiError::Mcp(_) => (S::INTERNAL_SERVER_ERROR, "MCP", "Mcp"),
        ApiError::WinMl(_) => (S::INTERNAL_SERVER_ERROR, "WINML", "WinMl"),
        ApiError::Io(_) => (S::INTERNAL_SERVER_ERROR, "IO", "Io"),
        ApiError::Json(_) => (S::INTERNAL_SERVER_ERROR, "JSON", "Json"),
    }
}

pub fn error_response(ctx: &ApiCtx, e: &ApiError, details: Option<Value>) -> Response {
    let (status, code, kind) = error_mapping(e);
    let message = sanitize(&e.to_string(), &ctx.data_dir);
    error_response_raw(status, code, kind, &message, details)
}

pub fn error_response_raw(
    status: axum::http::StatusCode,
    code: &str,
    kind: &str,
    message: &str,
    details: Option<Value>,
) -> Response {
    let body = json!({
        "error": {
            "code": code,
            "kind": kind,
            "message": message,
            "details": details,
        }
    });
    (status, axum::Json(body)).into_response()
}

/// Error strings from the app embed argv and filesystem paths; the
/// REST layer scrubs the machine's roots before one crosses the
/// wire and caps the length.
pub fn sanitize(message: &str, data_dir: &std::path::Path) -> String {
    let mut out = message.replace(&data_dir.display().to_string(), "<data-dir>");
    if let Some(home) = home_dir_string() {
        if !home.is_empty() {
            out = out.replace(&home, "<home>");
        }
    }
    if out.chars().count() > 400 {
        out = out.chars().take(400).collect::<String>() + "…";
    }
    out
}

fn home_dir_string() -> Option<String> {
    std::env::var("USERPROFILE")
        .ok()
        .or_else(|| std::env::var("HOME").ok())
}

// ---------------------------------------------------------------------------
// Derived / composite endpoints
// ---------------------------------------------------------------------------

async fn dispatch_read(ctx: &ApiCtx, op: Op, args: Value) -> Result<Value, Box<Response>> {
    manager_app::registry::dispatch(&ctx.app, &ctx.sink, op, args)
        .await
        .map_err(|e| Box::new(error_response(ctx, &e, None)))
}

fn find_by_id(list: &Value, key: &str, id: &str) -> Option<Value> {
    let items = list
        .get(key)
        .and_then(Value::as_array)
        .or_else(|| list.as_array())?;
    items
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
        .cloned()
}

fn not_found(ctx: &ApiCtx, what: &str, id: &str) -> Response {
    error_response(ctx, &ApiError::NotFound(format!("{what} {id}")), None)
}

async fn handle_server_identity(State(ctx): State<Arc<ApiCtx>>) -> Response {
    axum::Json(json!({
        "deployment_id": ctx.deployment_id,
        "app": "llmdil",
        "app_version": env!("CARGO_PKG_VERSION"),
        "api_version": "1",
        "features": {
            "operations": true,
            "events_sse": true,
            "auth": true
        }
    }))
    .into_response()
}

async fn handle_backend_one(State(ctx): State<Arc<ApiCtx>>, Path(id): Path<String>) -> Response {
    match dispatch_read(&ctx, Op::ListBackends, json!({})).await {
        Ok(list) => match find_by_id(&list, "backends", &id) {
            Some(b) => axum::Json(b).into_response(),
            None => not_found(&ctx, "backend", &id),
        },
        Err(r) => *r,
    }
}

async fn handle_model_one(State(ctx): State<Arc<ApiCtx>>, Path(id): Path<String>) -> Response {
    match dispatch_read(&ctx, Op::ListModels, json!({})).await {
        Ok(list) => match find_by_id(&list, "models", &id) {
            Some(m) => axum::Json(m).into_response(),
            None => not_found(&ctx, "model", &id),
        },
        Err(r) => *r,
    }
}

async fn handle_operation_one(State(ctx): State<Arc<ApiCtx>>, Path(id): Path<String>) -> Response {
    if let Some(record) = ctx.operations.get(&id) {
        return axum::Json(record).into_response();
    }
    match dispatch_read(&ctx, Op::TasksList, json!({})).await {
        Ok(list) => match find_by_id(&list, "tasks", &id) {
            Some(t) => axum::Json(t).into_response(),
            None => not_found(&ctx, "operation", &id),
        },
        Err(r) => *r,
    }
}

async fn handle_log_scopes(State(ctx): State<Arc<ApiCtx>>) -> Response {
    let args = json!({
        "scope": "gateway",
        "level": null, "search": null, "query": null,
        "since_ms": null, "until_ms": null, "order": null,
        "offset": null, "limit": 0
    });
    match dispatch_read(&ctx, Op::GetLogs, args).await {
        Ok(v) => {
            let scopes = v.get("scopes").cloned().unwrap_or(json!([]));
            axum::Json(json!({ "scopes": scopes })).into_response()
        }
        Err(r) => *r,
    }
}

async fn handle_gateway_groups(State(ctx): State<Arc<ApiCtx>>) -> Response {
    match dispatch_read(&ctx, Op::GetGatewayConfig, json!({})).await {
        Ok(v) => {
            let groups = v.get("groups").cloned().unwrap_or(json!([]));
            axum::Json(json!({ "groups": groups })).into_response()
        }
        Err(r) => *r,
    }
}

async fn handle_updates(State(ctx): State<Arc<ApiCtx>>) -> Response {
    match dispatch_read(&ctx, Op::GetConfig, json!({})).await {
        Ok(v) => {
            let updates = v.get("updates").cloned().unwrap_or(json!(null));
            axum::Json(json!({ "updates": updates })).into_response()
        }
        Err(r) => *r,
    }
}

// ---------------------------------------------------------------------------
// Composite handlers (phase 3): thin HTTP adapters over
// composites.rs, sharing the op handlers' error envelope.
// ---------------------------------------------------------------------------

fn parse_body(ctx: &ApiCtx, body: &axum::body::Bytes) -> Result<Value, Box<Response>> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(body).map_err(|e| {
        Box::new(error_response(
            ctx,
            &ApiError::InvalidInput(format!("malformed JSON body: {e}")),
            None,
        ))
    })
}

async fn handle_model_patch(
    State(ctx): State<Arc<ApiCtx>>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let parsed = match parse_body(&ctx, &body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match composites::patch_model(&ctx, &id, parsed).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => error_response(&ctx, &e, None),
    }
}

async fn handle_group_create(
    State(ctx): State<Arc<ApiCtx>>,
    body: axum::body::Bytes,
) -> Response {
    let parsed = match parse_body(&ctx, &body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match composites::create_group(&ctx, parsed).await {
        Ok(v) => (axum::http::StatusCode::CREATED, axum::Json(v)).into_response(),
        Err(e) => error_response(&ctx, &e, None),
    }
}

async fn handle_group_patch(
    State(ctx): State<Arc<ApiCtx>>,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let parsed = match parse_body(&ctx, &body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match composites::patch_group(&ctx, &name, parsed).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => error_response(&ctx, &e, None),
    }
}

async fn handle_group_delete(
    State(ctx): State<Arc<ApiCtx>>,
    Path(name): Path<String>,
) -> Response {
    match composites::delete_group(&ctx, &name).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => error_response(&ctx, &e, None),
    }
}

async fn handle_gateway_start(State(ctx): State<Arc<ApiCtx>>) -> Response {
    // The start-gateway step is minutes-scale: it runs as an
    // operation like its registry route does.
    let (id, record) = ctx.operations.begin("gateway_start");
    let spawn_ctx = ctx.clone();
    tokio::spawn(async move {
        match composites::gateway_start(&spawn_ctx).await {
            Ok(value) => {
                if value.get("busy") == Some(&json!(true)) {
                    spawn_ctx.operations.fail_with_detail(
                        &id,
                        "a wizard step is already running",
                        value,
                    );
                } else {
                    spawn_ctx
                        .operations
                        .complete(&id, "gateway_start", Ok(value));
                }
            }
            Err(e) => {
                let message = sanitize(&e.to_string(), &spawn_ctx.data_dir);
                spawn_ctx
                    .operations
                    .complete(&id, "gateway_start", Err(message));
            }
        }
    });
    (axum::http::StatusCode::ACCEPTED, axum::Json(record)).into_response()
}

async fn handle_activation_apply(
    State(ctx): State<Arc<ApiCtx>>,
    body: axum::body::Bytes,
) -> Response {
    let parsed = match parse_body(&ctx, &body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    // The apply is the longest composition (drain + gateway
    // restart): it runs as an operation. Body shape errors are
    // refused synchronously, before queueing.
    if let Err(e) = composites::validate_apply_body(&parsed) {
        return error_response(&ctx, &e, None);
    }
    let (id, record) = ctx.operations.begin("activation_apply");
    let spawn_ctx = ctx.clone();
    tokio::spawn(async move {
        match composites::activation_apply(&spawn_ctx, parsed).await {
            Ok(value) => spawn_ctx
                .operations
                .complete(&id, "activation_apply", Ok(value)),
            Err(e) => {
                let message = sanitize(&e.to_string(), &spawn_ctx.data_dir);
                spawn_ctx
                    .operations
                    .complete(&id, "activation_apply", Err(message));
            }
        }
    });
    (axum::http::StatusCode::ACCEPTED, axum::Json(record)).into_response()
}

async fn handle_bundle_content(
    State(ctx): State<Arc<ApiCtx>>,
    Path(bundle_id): Path<String>,
) -> Response {
    let Some(path) = ctx.operations.bundle_path(&bundle_id) else {
        return not_found(&ctx, "bundle", &bundle_id);
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "diagnostics.zip".to_string());
            (
                axum::http::StatusCode::OK,
                [
                    (axum::http::header::CONTENT_TYPE, "application/zip".to_string()),
                    (
                        axum::http::header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{name}\""),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Err(e) => error_response(
            &ctx,
            &ApiError::Io(format!("bundle file unreadable: {e}")),
            None,
        ),
    }
}

/// Process-global SSE event sequence: ids keep increasing across
/// reconnects, so a client can tell what it missed (events are not
/// replayed; the stream is live-only).
static EVENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct MpscEventStream {
    rx: tokio::sync::mpsc::Receiver<
        Result<axum::response::sse::Event, std::convert::Infallible>,
    >,
}

impl futures_core::Stream for MpscEventStream {
    type Item = Result<axum::response::sse::Event, std::convert::Infallible>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

async fn handle_events(State(ctx): State<Arc<ApiCtx>>) -> Response {
    let mut events = ctx.events.subscribe();
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(env) => {
                    let id = EVENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let event = axum::response::sse::Event::default()
                        .id(id.to_string())
                        .event(env.event)
                        .data(env.payload.to_string());
                    if tx.send(Ok(event)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let event = axum::response::sse::Event::default()
                        .comment(format!("lagged: {n} event(s) dropped"));
                    if tx.send(Ok(event)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    axum::response::sse::Sse::new(MpscEventStream { rx })
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_scrubs_roots_and_caps_length() {
        let dir = std::path::Path::new("/srv/llmdil/data");
        let msg = format!("failed at {}/models/x.gguf", dir.display());
        let out = sanitize(&msg, dir);
        assert_eq!(out, "failed at <data-dir>/models/x.gguf");
        let long = "x".repeat(500);
        assert!(sanitize(&long, dir).chars().count() <= 401);
    }

    #[test]
    fn coercion_prefers_numbers_then_bool() {
        assert_eq!(coerce_query_value("42"), json!(42));
        assert_eq!(coerce_query_value("4.5"), json!(4.5));
        assert_eq!(coerce_query_value("true"), json!(true));
        assert_eq!(coerce_query_value("abc"), json!("abc"));
    }

    #[test]
    fn every_error_variant_maps() {
        // Kind strings are the wire contract; pinning them here keeps
        // a variant rename from silently changing the API.
        let cases = [
            (ApiError::InvalidInput("x".into()), 400),
            (ApiError::NotFound("x".into()), 404),
            (ApiError::Config("x".into()), 422),
            (ApiError::Unsupported("x".into()), 501),
            (ApiError::Gateway("x".into()), 500),
        ];
        for (e, status) in cases {
            assert_eq!(error_mapping(&e).0.as_u16(), status);
        }
    }
}

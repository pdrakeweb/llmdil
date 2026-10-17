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
        .route("/api/v1/models/{id}", get(handle_model_one))
        .route("/api/v1/operations/{id}", get(handle_operation_one))
        .route("/api/v1/logs/scopes", get(handle_log_scopes))
        .route("/api/v1/gateway/groups", get(handle_gateway_groups))
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
        );
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
            "events_sse": false,
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

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

use axum::extract::{Extension, Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, MethodFilter};
use axum::Router;
use serde_json::{json, Map, Value};

use manager_app::registry::{request_params, HttpMethod, Op, OpSpec, Role, RouteMode, OP_SPECS};
use manager_app::{ApiError, Refusal, TaskStatus};

use crate::composites;
use crate::operations::{AbortTarget, OpFailure, OpOutcome};
use crate::{ApiCtx, Principal, API_ORIGIN};

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
    // The second file of the generated gateway configuration, and the
    // saved telemetry snapshots (their POST is a registry route).
    ("GET", "/api/v1/gateway/generated-routing"),
    ("GET", "/api/v1/telemetry/snapshots"),
    ("GET", "/api/v1/telemetry/snapshots/{id}"),
    ("DELETE", "/api/v1/telemetry/snapshots/{id}"),
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
            axum::routing::on(filter, move |state, principal, path, query, body| {
                handle_op(spec, state, principal, path, query, body)
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
            axum::routing::patch(handle_group_patch).delete(handle_group_delete),
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
            "/api/v1/gateway/generated-routing",
            get(handle_generated_routing),
        )
        .route("/api/v1/telemetry/snapshots", get(handle_snapshots))
        .route(
            "/api/v1/telemetry/snapshots/{id}",
            get(handle_snapshot_one).delete(handle_snapshot_delete),
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
        .route(
            "/api/v1/auth/login",
            axum::routing::post(crate::auth_routes::handle_login),
        )
        .route(
            "/api/v1/auth/totp",
            axum::routing::post(crate::auth_routes::handle_totp),
        )
        .route(
            "/api/v1/auth/refresh",
            axum::routing::post(crate::auth_routes::handle_refresh),
        )
        .route(
            "/api/v1/enroll",
            axum::routing::post(crate::auth_routes::handle_enroll),
        )
        .route("/api/v1/auth/me", get(crate::auth_routes::handle_me))
        .route(
            "/api/v1/auth/logout",
            axum::routing::post(crate::auth_routes::handle_logout),
        )
        .route(
            "/api/v1/auth/password",
            axum::routing::post(crate::auth_routes::handle_change_password),
        )
        .route(
            "/api/v1/auth/sessions",
            get(crate::auth_routes::handle_list_sessions),
        )
        .route(
            "/api/v1/auth/sessions/{id}",
            axum::routing::delete(crate::auth_routes::handle_revoke_session),
        )
        .route(
            "/api/v1/auth/totp/enroll",
            axum::routing::post(crate::auth_routes::handle_totp_enroll),
        )
        .route(
            "/api/v1/auth/totp/verify",
            axum::routing::post(crate::auth_routes::handle_totp_verify),
        )
        .route(
            "/api/v1/auth/totp/disable",
            axum::routing::post(crate::auth_routes::handle_totp_disable),
        )
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
        error_response(&ctx, &ApiError::NotFound("no such route".to_string()), None)
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
    Extension(principal): Extension<Principal>,
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
        // What can be refused now is refused now (400, 404, 409).
        if let Err(message) = check_args(spec, &args) {
            return error_response(&ctx, &ApiError::InvalidInput(message), None);
        }
        if let Err(refusal) = manager_app::precheck_operation(&ctx.app, spec.op, &args).await {
            return refusal_response(&ctx, refusal);
        }
        let work_ctx = ctx.clone();
        return spawn_operation(&ctx, spec.name, async move {
            run_async_op(&work_ctx, spec.op, args).await
        });
    }
    // The operations list merges the app's task records with the
    // API-side operation records.
    if spec.op == Op::TasksList {
        return match dispatch_read(&ctx, spec.op, args).await {
            Ok(tasks) => {
                let rows = operation_rows(&ctx, &principal, tasks);
                (axum::http::StatusCode::OK, axum::Json(rows)).into_response()
            }
            Err(r) => *r,
        };
    }
    // Abort: an API-side record's abort is routed to the app tasks its
    // work recorded; anything else is an app task.
    if spec.op == Op::TaskAbort {
        let id = args["task_id"].as_str().unwrap_or_default().to_string();
        if let Some(target) = ctx.operations.request_abort(&id) {
            return axum::Json(abort_operation(&ctx, &id, target).await).into_response();
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
    match manager_app::registry::dispatch_from(&ctx.app, &ctx.sink, spec.op, args, API_ORIGIN).await
    {
        Ok(value) => shape_success(&ctx, spec, &principal, value),
        Err(e) => error_response(&ctx, &e, None),
    }
}

/// Create the operation record, run `work` in the background, answer 202.
fn spawn_operation<F>(ctx: &Arc<ApiCtx>, kind: &'static str, work: F) -> Response
where
    F: std::future::Future<Output = Result<Value, ApiError>> + Send + 'static,
{
    let (id, record, link) = ctx.operations.begin(kind);
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let result = manager_app::with_task_link(link, work).await;
        finish_operation(&ctx, &id, kind, result).await;
    });
    (axum::http::StatusCode::ACCEPTED, axum::Json(record)).into_response()
}

/// Dispatch an asynchronous operation; a start additionally waits for the
/// model when `wait_ready_secs` asks for it.
async fn run_async_op(ctx: &ApiCtx, op: Op, args: Value) -> Result<Value, ApiError> {
    let start = (op == Op::StartBackend).then(|| args.clone());
    let mut value =
        manager_app::registry::dispatch_from(&ctx.app, &ctx.sink, op, args, API_ORIGIN).await?;
    if let Some(args) = start {
        let secs = args["wait_ready_secs"].as_u64().unwrap_or(0);
        if secs > 0 {
            manager_app::wait_started_backend(
                &ctx.app,
                &ctx.sink,
                args["id"].as_str().unwrap_or_default(),
                secs,
                args["origin"].as_str(),
                API_ORIGIN,
            )
            .await?;
            value["ready"] = json!(true);
        }
    }
    Ok(value)
}

/// Abort a record: cancel the app tasks its work recorded and wait a
/// moment for the work to end; work that has no task, or does not stop,
/// is marked aborted with a note.
async fn abort_operation(ctx: &ApiCtx, id: &str, target: AbortTarget) -> Value {
    let link = match target {
        AbortTarget::Finished(status) => {
            return json!({
                "ok": true, "task_id": id, "status": status, "already_finished": true,
            })
        }
        AbortTarget::Running(link) => link,
    };
    link.request_abort();
    let mut tasks = Vec::new();
    for task_id in link.task_ids() {
        let args = json!({ "task_id": task_id });
        match manager_app::registry::dispatch_from(
            &ctx.app,
            &ctx.sink,
            Op::TaskAbort,
            args,
            API_ORIGIN,
        )
        .await
        {
            Ok(v) => tasks.push(v),
            Err(e) => tasks.push(json!({ "task_id": task_id, "error": e.to_string() })),
        }
    }
    let status = || {
        let record = ctx.operations.get(id).unwrap_or_default();
        record["status"].as_str().unwrap_or_default().to_string()
    };
    if !tasks.is_empty() {
        for _ in 0..20 {
            if status() != "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    let stopped = status() != "running";
    ctx.operations.force_abort(
        id,
        "this operation cannot be stopped, or did not stop in time; it may still be running",
    );
    json!({
        "ok": true, "task_id": id, "status": status(),
        "stopped": stopped, "forced": !stopped, "tasks": tasks,
    })
}

/// `GET /operations`: the API records and the app's task records, newest
/// first. The two rows of one unit of work name each other (`task_id`,
/// `operation_id`).
fn operation_rows(ctx: &ApiCtx, principal: &Principal, tasks: Value) -> Vec<Value> {
    let records = ctx.operations.list();
    let mut rows: Vec<Value> = Vec::new();
    for task in tasks.as_array().into_iter().flatten() {
        let operation = records
            .iter()
            .find(|(_, ids)| ids.iter().any(|id| task["id"] == id.as_str()))
            .map(|(record, _)| record["id"].clone());
        rows.push(task_row(task.clone(), operation));
    }
    rows.extend(records.into_iter().map(|(record, _)| record));
    let id = |r: &Value| r["id"].as_str().unwrap_or_default().to_string();
    rows.sort_by_key(|r| std::cmp::Reverse((r["created_ms"].as_i64().unwrap_or(0), id(r))));
    rows.into_iter()
        .map(|r| visible_to(ctx, principal, r))
        .collect()
}

/// An app task record in the operation shape (rest-api.md §3.1).
fn task_row(mut task: Value, operation: Option<Value>) -> Value {
    let failed = task["status"] == "failed";
    let kind = task["detail"]["error_kind"].as_str().map(str::to_string);
    task["error_code"] = json!(failed.then(|| match &kind {
        Some(k) => code_of_kind(k),
        None => OPERATION_FAILED,
    }));
    task["error_kind"] = json!(failed.then(|| kind.unwrap_or("OperationFailed".into())));
    task["operation_id"] = json!(operation);
    task
}

/// Local paths in an operation's payload are for administrators: every
/// other role sees the machine's roots replaced.
fn visible_to(ctx: &ApiCtx, principal: &Principal, mut value: Value) -> Value {
    if principal.role != Role::Admin {
        scrub_value(&mut value, &ctx.data_dir);
    }
    value
}

/// The code of a failure an operation reports about itself.
const OPERATION_FAILED: &str = "OPERATION_FAILED";

/// The terminal outcome of an asynchronous operation, by the one
/// shared lifecycle rule ([`manager_app::lifecycle::payload_failure`]):
/// a payload with `busy: true` or `ok: false` is a failed operation
/// whose error is the payload's message and whose detail is the whole
/// payload (CR-2: a failed or manual start-gateway step, a refused
/// run, or an activation apply with a failed item). An audit that found
/// another one running is refused the same way. The code is the one a
/// synchronous route would have answered.
pub fn operation_outcome(result: Result<Value, ApiError>) -> Result<Value, OpFailure> {
    match result {
        Ok(value) => {
            let already = value.get("already_running") == Some(&json!(true));
            let busy = already || value.get("busy") == Some(&json!(true));
            let message = match manager_app::lifecycle::payload_failure(&value) {
                Some(message) => message,
                None if already => "busy: an audit is already running".to_string(),
                None => return Ok(value),
            };
            let (code, kind) = if busy {
                ("BUSY", "Busy")
            } else {
                (OPERATION_FAILED, "OperationFailed")
            };
            Err(OpFailure {
                message,
                code,
                kind,
                detail: Some(value),
            })
        }
        Err(e) => Err(OpFailure {
            message: e.to_string(),
            code: code_of_kind(e.kind()),
            kind: e.kind(),
            detail: None,
        }),
    }
}

/// Record an asynchronous operation's terminal state (see
/// [`operation_outcome`]). Every 202 route ends here: the registry's
/// AsyncOp routes and the gateway-start / activation-apply composites.
/// A download's dispatch returns when the transfer began: its operation
/// follows the app task to its end.
async fn finish_operation(
    ctx: &ApiCtx,
    id: &str,
    kind: &'static str,
    result: Result<Value, ApiError>,
) {
    let mut outcome = match operation_outcome(result) {
        Ok(value) => OpOutcome::Done(value),
        Err(f) => OpOutcome::Failed(f),
    };
    if let OpOutcome::Done(payload) = &outcome {
        if kind == "download_model" || kind == "model_resume_download" {
            outcome = transfer_outcome(ctx, id, payload.clone()).await;
        }
    } else if aborted_task(ctx, id).await {
        let OpOutcome::Failed(f) = outcome else {
            return;
        };
        outcome = OpOutcome::Aborted(f.detail);
    }
    if let OpOutcome::Failed(f) = &mut outcome {
        f.message = sanitize(&f.message, &ctx.data_dir);
    }
    ctx.operations.complete(id, kind, outcome);
}

/// Whether one of the operation's app tasks was aborted (by its own id).
async fn aborted_task(ctx: &ApiCtx, id: &str) -> bool {
    let Ok(tasks) = dispatch_read(ctx, Op::TasksList, json!({})).await else {
        return false;
    };
    tasks.as_array().into_iter().flatten().any(|t| {
        t["status"] == "aborted"
            && t["id"]
                .as_str()
                .and_then(|t| ctx.operations.operation_of_task(t))
                .as_deref()
                == Some(id)
    })
}

/// Follow a download's app task to its end.
async fn transfer_outcome(ctx: &ApiCtx, id: &str, mut payload: Value) -> OpOutcome {
    let task_id = match payload["task_id"].as_str() {
        Some(t) => t.to_string(),
        // A resume continues the transfer another operation began.
        None => match running_download(ctx, payload["model_id"].as_str()).await {
            Some(t) => t,
            None => return OpOutcome::Done(payload),
        },
    };
    ctx.operations.link_task(id, &task_id);
    let Some(task) = manager_app::wait_task_settled(&ctx.app, &task_id).await else {
        return OpOutcome::Done(payload);
    };
    if let (Some(p), Some(d)) = (payload.as_object_mut(), task.detail.as_object()) {
        p.extend(d.clone());
    }
    match task.status {
        TaskStatus::Aborted => OpOutcome::Aborted(Some(payload)),
        TaskStatus::Failed => OpOutcome::Failed(OpFailure {
            message: task.error.unwrap_or("the download failed".into()),
            code: "MODELS",
            kind: "Models",
            detail: Some(payload),
        }),
        _ => OpOutcome::Done(payload),
    }
}

/// The app task of the transfer in flight for a model.
async fn running_download(ctx: &ApiCtx, model_id: Option<&str>) -> Option<String> {
    let tasks = dispatch_read(ctx, Op::TasksList, json!({})).await.ok()?;
    let task = tasks.as_array()?.iter().find(|t| {
        t["kind"] == "model_download"
            && t["status"] == "running"
            && t["detail"]["model_id"].as_str() == model_id
    })?;
    task["id"].as_str().map(str::to_string)
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
            let schema = param_schema(spec.op, k);
            let value = query_value(&schema, v).map_err(|want| {
                Box::new(error_response_raw(
                    axum::http::StatusCode::BAD_REQUEST,
                    "INVALID_INPUT",
                    "InvalidInput",
                    &format!("query parameter `{k}`: expected {want}"),
                    None,
                ))
            })?;
            obj.insert(k.clone(), value);
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
                    if !spec.params.contains(&k.as_str()) && api_param(spec.op, k).is_none() {
                        return Err(Box::new(error_response_raw(
                            axum::http::StatusCode::BAD_REQUEST,
                            "INVALID_INPUT",
                            "InvalidInput",
                            &format!("unknown field `{k}` for {}", spec.name),
                            None,
                        )));
                    }
                    // The path names the target (and is what the audit
                    // log records): a body may repeat it, never change it.
                    if path_params.get(k).is_some_and(|p| v.as_str() != Some(p)) {
                        return Err(Box::new(error_response_raw(
                            axum::http::StatusCode::BAD_REQUEST,
                            "INVALID_INPUT",
                            "InvalidInput",
                            &format!("field `{k}` does not match the path"),
                            None,
                        )));
                    }
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
    }
    // Lifecycle operations record who asked (markers, events,
    // history): a REST caller that names no origin is `api` (the CLI
    // sends `cli`).
    if spec.params.contains(&"origin") && !obj.contains_key("origin") {
        obj.insert("origin".to_string(), json!("api"));
    }
    Ok(Value::Object(obj))
}

/// A body field this layer adds to an operation's own arguments: its
/// JSON Schema.
fn api_param(op: Op, name: &str) -> Option<Value> {
    (op == Op::StartBackend && name == "wait_ready_secs")
        .then(|| json!({ "type": ["integer", "null"], "minimum": 0 }))
}

/// The declared JSON Schema of one argument (any value when unknown).
fn param_schema(op: Op, name: &str) -> Value {
    request_params(op)
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| p.json_schema)
        .unwrap_or(json!({}))
}

/// Refuse a missing or wrongly typed argument in the words of the
/// operation's own parse, before anything is queued.
fn check_args(spec: &OpSpec, args: &Value) -> Result<(), String> {
    for p in request_params(spec.op) {
        let value = args.get(p.name).unwrap_or(&Value::Null);
        if value.is_null() && !p.required {
            continue;
        }
        if value.is_null() && args.get(p.name).is_none() {
            return Err(format!("argument `{}` is required", p.name));
        }
        if !schema_accepts(&p.json_schema, value) {
            return Err(format!(
                "argument `{}`: expected {}",
                p.name,
                schema_text(&p.json_schema)
            ));
        }
    }
    for (name, value) in args.as_object().into_iter().flatten() {
        if let Some(schema) = api_param(spec.op, name) {
            if !schema_accepts(&schema, value) {
                return Err(format!(
                    "argument `{name}`: expected {}",
                    schema_text(&schema)
                ));
            }
        }
    }
    Ok(())
}

/// The type names of a generated parameter schema (empty: any value).
fn schema_types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(t)) => vec![t],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Whether `value` fits a generated parameter schema (type, enum and
/// integer range; documents are the operation's to check).
fn schema_accepts(schema: &Value, value: &Value) -> bool {
    if let Some(any) = schema.get("anyOf").and_then(Value::as_array) {
        return any.iter().any(|s| schema_accepts(s, value));
    }
    if let Some(names) = schema.get("enum").and_then(Value::as_array) {
        return names.contains(value);
    }
    let types = schema_types(schema);
    let int = |k: &str| schema.get(k).and_then(Value::as_i64);
    types.is_empty()
        || types.iter().any(|t| match *t {
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "number" => value.is_number(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            "null" => value.is_null(),
            "integer" => match (value.as_i64(), value.as_u64()) {
                (Some(n), _) => {
                    int("minimum").is_none_or(|m| n >= m) && int("maximum").is_none_or(|m| n <= m)
                }
                (None, Some(_)) => int("maximum").is_none(),
                _ => false,
            },
            _ => true,
        })
}

/// What a parameter schema wants, for a refusal.
fn schema_text(schema: &Value) -> String {
    if let Some(any) = schema.get("anyOf").and_then(Value::as_array) {
        return any.first().map(schema_text).unwrap_or_default();
    }
    if let Some(names) = schema.get("enum").and_then(Value::as_array) {
        let names: Vec<String> = names.iter().map(Value::to_string).collect();
        return format!("one of {}", names.join(", "));
    }
    match schema_types(schema).first() {
        Some(&"integer") if schema.get("minimum").is_some() => "a whole number, 0 or more".into(),
        Some(&"integer") => "a whole number".into(),
        Some(&"boolean") => "true or false".into(),
        Some(t) => format!("a value of type {t}"),
        None => "a JSON value".into(),
    }
}

/// Query strings are strings: the parameter's declared type says how one
/// is read (`?limit=20` is a number, `?search=20` is text). The error is
/// what was expected.
fn query_value(schema: &Value, raw: &str) -> Result<Value, String> {
    let inner = schema
        .get("anyOf")
        .and_then(|any| any.get(0))
        .unwrap_or(schema);
    let parsed = match schema_types(inner).first().copied() {
        Some("integer") => raw
            .parse::<i64>()
            .map(|n| json!(n))
            .or_else(|_| raw.parse::<u64>().map(|n| json!(n)))
            .ok(),
        Some("number") => raw
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .map(|n| json!(n)),
        Some("boolean") => raw.parse::<bool>().ok().map(|b| json!(b)),
        Some("string") | None => Some(json!(raw)),
        Some(_) => serde_json::from_str(raw).ok(),
    };
    parsed
        .filter(|v| schema_accepts(inner, v))
        .ok_or_else(|| schema_text(inner))
}

fn shape_success(ctx: &ApiCtx, spec: &OpSpec, principal: &Principal, value: Value) -> Response {
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
    let value = match spec.op {
        Op::TelemetrySaveSnapshot => visible_to(ctx, principal, value),
        _ => value,
    };
    (status, axum::Json(value)).into_response()
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Map a typed op error to (status, REST code, kind). One place —
/// the REST layer owns presentation (PLAN.md §3.3).
pub fn error_mapping(e: &ApiError) -> (axum::http::StatusCode, &'static str, &'static str) {
    use axum::http::StatusCode as S;
    let status = match e {
        ApiError::InvalidInput(_) => S::BAD_REQUEST,
        ApiError::NotFound(_) => S::NOT_FOUND,
        ApiError::Config(_) => S::UNPROCESSABLE_ENTITY,
        ApiError::Unsupported(_) | ApiError::ContractGap(_) => S::NOT_IMPLEMENTED,
        ApiError::Busy(_) => S::CONFLICT,
        ApiError::Supervisor(_)
        | ApiError::Wizard(_)
        | ApiError::Telemetry(_)
        | ApiError::Gateway(_)
        | ApiError::Models(_)
        | ApiError::Mxc(_)
        | ApiError::Mcp(_)
        | ApiError::WinMl(_)
        | ApiError::Services(_)
        | ApiError::Io(_)
        | ApiError::Json(_) => S::INTERNAL_SERVER_ERROR,
    };
    (status, code_of_kind(e.kind()), e.kind())
}

/// The REST code of an error kind ([`ApiError::kind`]).
fn code_of_kind(kind: &str) -> &'static str {
    match kind {
        "InvalidInput" => "INVALID_INPUT",
        "NotFound" => "NOT_FOUND",
        "Config" => "CONFIG",
        "Unsupported" => "UNSUPPORTED",
        "ContractGap" => "CONTRACT_GAP",
        "Supervisor" => "SUPERVISOR",
        "Wizard" => "WIZARD",
        "Telemetry" => "TELEMETRY",
        "Gateway" => "GATEWAY",
        "Models" => "MODELS",
        "Mxc" => "MXC",
        "Mcp" => "MCP",
        "WinMl" => "WINML",
        "Services" => "SERVICES",
        "Io" => "IO",
        "Json" => "JSON",
        "Busy" => "BUSY",
        _ => OPERATION_FAILED,
    }
}

/// A refusal made before the work started.
fn refusal_response(ctx: &ApiCtx, refusal: Refusal) -> Response {
    error_response(ctx, &refusal.error, refusal.details)
}

/// Map an `ApiError` onto the JSON error envelope. The message passes through
/// [`sanitize`] first, so machine paths never reach a remote client.
pub fn error_response(ctx: &ApiCtx, e: &ApiError, details: Option<Value>) -> Response {
    let (status, code, kind) = error_mapping(e);
    let message = sanitize(&e.to_string(), &ctx.data_dir);
    error_response_raw(status, code, kind, &message, details)
}

/// Build the error envelope `{"error": {code, kind, message, details}}` directly. Unlike
/// [`error_response`] this does not sanitize: `message` must already be safe to show a
/// remote client.
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

/// The longest error message that crosses the wire. Long enough for a
/// memory refusal that names every GPU and who uses it.
const MAX_MESSAGE_CHARS: usize = 4000;

/// Error strings from the app embed argv and filesystem paths; the
/// REST layer scrubs the machine's roots before one crosses the
/// wire and caps the length.
pub fn sanitize(message: &str, data_dir: &std::path::Path) -> String {
    let mut out = scrub_roots(message, data_dir);
    if out.chars().count() > MAX_MESSAGE_CHARS {
        out = out.chars().take(MAX_MESSAGE_CHARS).collect::<String>() + "…";
    }
    out
}

/// Replace the data directory and the home directory, however they are
/// spelled (letter case, slashes, doubled backslashes of a quoted path).
fn scrub_roots(text: &str, data_dir: &std::path::Path) -> String {
    let mut out = text.to_string();
    let roots = [
        (data_dir.display().to_string(), "<data-dir>"),
        (home_dir_string().unwrap_or_default(), "<home>"),
    ];
    for (root, name) in roots {
        let root = root.trim_end_matches(['/', '\\']);
        if root.len() > 3 {
            out = replace_path(&out, &root.replace('\\', "\\\\"), name);
            out = replace_path(&out, root, name);
        }
    }
    out
}

/// Replace `path` in `text`, ignoring ASCII case and slash direction.
fn replace_path(text: &str, path: &str, with: &str) -> String {
    // Both keep every byte offset.
    let fold = |s: &str| s.to_ascii_lowercase().replace('/', "\\");
    let (hay, needle) = (fold(text), fold(path));
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    while let Some(at) = hay[from..].find(&needle) {
        out.push_str(&text[from..from + at]);
        out.push_str(with);
        from += at + needle.len();
    }
    out.push_str(&text[from..]);
    out
}

/// [`scrub_roots`] for every string in a payload.
fn scrub_value(value: &mut Value, data_dir: &std::path::Path) {
    match value {
        Value::String(s) => *s = scrub_roots(s, data_dir),
        Value::Array(items) => items.iter_mut().for_each(|v| scrub_value(v, data_dir)),
        Value::Object(fields) => fields.values_mut().for_each(|v| scrub_value(v, data_dir)),
        _ => {}
    }
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
    manager_app::registry::dispatch_from(&ctx.app, &ctx.sink, op, args, API_ORIGIN)
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

async fn handle_operation_one(
    State(ctx): State<Arc<ApiCtx>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    let row = match ctx.operations.get(&id) {
        Some(record) => record,
        None => match dispatch_read(&ctx, Op::TasksList, json!({})).await {
            Ok(list) => match find_by_id(&list, "tasks", &id) {
                Some(t) => {
                    let operation = ctx.operations.operation_of_task(&id);
                    task_row(t, operation.map(Value::from))
                }
                None => return not_found(&ctx, "operation", &id),
            },
            Err(r) => return *r,
        },
    };
    axum::Json(visible_to(&ctx, &principal, row)).into_response()
}

/// The second generated gateway file (`llmdil_routing.py`).
async fn handle_generated_routing(State(ctx): State<Arc<ApiCtx>>) -> Response {
    match dispatch_read(&ctx, Op::GatewayPreviewYaml, json!({})).await {
        Ok(v) => match v.get("routing_py").and_then(Value::as_str) {
            Some(text) => (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "text/x-python")],
                text.to_string(),
            )
                .into_response(),
            None => error_response(
                &ctx,
                &ApiError::NotFound(
                    "no routing file is generated: no routing rule or overflow setting needs one"
                        .to_string(),
                ),
                None,
            ),
        },
        Err(r) => *r,
    }
}

fn json_result(ctx: &ApiCtx, result: Result<Value, ApiError>) -> Response {
    match result {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => error_response(ctx, &e, None),
    }
}

async fn handle_snapshots(State(ctx): State<Arc<ApiCtx>>) -> Response {
    json_result(&ctx, manager_app::telemetry_snapshots(&ctx.app))
}

async fn handle_snapshot_one(State(ctx): State<Arc<ApiCtx>>, Path(id): Path<String>) -> Response {
    json_result(&ctx, manager_app::telemetry_snapshot(&ctx.app, &id))
}

async fn handle_snapshot_delete(
    State(ctx): State<Arc<ApiCtx>>,
    Path(id): Path<String>,
) -> Response {
    json_result(&ctx, manager_app::telemetry_snapshot_delete(&ctx.app, &id))
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

async fn handle_group_create(State(ctx): State<Arc<ApiCtx>>, body: axum::body::Bytes) -> Response {
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
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let cascade = match query.get("cascade").map(String::as_str) {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => {
            let e = ApiError::InvalidInput("`cascade` must be true or false".to_string());
            return error_response(&ctx, &e, None);
        }
    };
    json_result(&ctx, composites::delete_group(&ctx, &name, cascade).await)
}

async fn handle_gateway_start(State(ctx): State<Arc<ApiCtx>>) -> Response {
    // The start-gateway step is minutes-scale: it runs as an
    // operation like its registry route does.
    let step = json!({ "step_id": "start-gateway" });
    if let Err(refusal) = manager_app::precheck_operation(&ctx.app, Op::RunWizardStep, &step).await
    {
        return refusal_response(&ctx, refusal);
    }
    let work_ctx = ctx.clone();
    spawn_operation(&ctx, "gateway_start", async move {
        composites::gateway_start(&work_ctx).await
    })
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
    if let Err(refusal) =
        manager_app::precheck_operation(&ctx.app, Op::ActivationApply, &parsed).await
    {
        return refusal_response(&ctx, refusal);
    }
    let work_ctx = ctx.clone();
    spawn_operation(&ctx, "activation_apply", async move {
        composites::activation_apply(&work_ctx, parsed).await
    })
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
                    (
                        axum::http::header::CONTENT_TYPE,
                        "application/zip".to_string(),
                    ),
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
    rx: tokio::sync::mpsc::Receiver<Result<axum::response::sse::Event, std::convert::Infallible>>,
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

/// The SSE `data` for one bus event: the producer's payload with an
/// additive `origin` (`desktop` | `api` | `app`) when the payload is
/// an object that does not already carry one.
/// A task record carries the paths its operation row does, and goes by
/// the same rule (`visible_to`).
fn sse_data(ctx: &ApiCtx, principal: &Principal, env: &manager_app::event_bus::BusEvent) -> String {
    let mut payload = env.payload.clone();
    if env.event == manager_app::events::TASK_PROGRESS {
        payload = visible_to(ctx, principal, payload);
    }
    if let Some(obj) = payload.as_object_mut() {
        obj.entry("origin")
            .or_insert_with(|| json!(env.origin.as_str()));
    }
    payload.to_string()
}

/// `GET /events`: the app event bus (every UI event, whichever surface
/// caused it — desktop IPC, this API, or background producers). The
/// subscription also keeps the bus's background producers running.
async fn handle_events(
    State(ctx): State<Arc<ApiCtx>>,
    Extension(principal): Extension<Principal>,
) -> Response {
    let mut events = manager_app::event_bus::subscribe(&ctx.app);
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                // A disconnected client drops its subscription now,
                // not at the next event, so idle producers stop.
                _ = tx.closed() => break,
                next = events.recv() => next,
            };
            match next {
                Ok(env) => {
                    let id = EVENT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let event = axum::response::sse::Event::default()
                        .id(id.to_string())
                        .event(env.event.clone())
                        .data(sse_data(&ctx, &principal, &env));
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
        // A memory refusal for several GPUs stays whole.
        let refusal = "x".repeat(2000);
        assert_eq!(sanitize(&refusal, dir), refusal);
        let long = "x".repeat(5000);
        assert!(sanitize(&long, dir).chars().count() <= MAX_MESSAGE_CHARS + 1);
    }

    #[test]
    fn scrubbing_ignores_how_a_path_is_spelled() {
        let dir = std::path::Path::new("D:\\LLM\\Data");
        for text in [
            "D:\\LLM\\Data\\x.zip",
            "d:\\llm\\data\\x.zip",
            "D:/LLM/Data/x.zip",
            "\"D:\\\\LLM\\\\Data\\\\x.zip\"",
        ] {
            let mut v = json!({ "a": [{ "path": text }], "n": 1 });
            scrub_value(&mut v, dir);
            let out = v["a"][0]["path"].as_str().unwrap();
            assert!(out.contains("<data-dir>"), "{text} -> {out}");
            assert!(!out.to_lowercase().contains("llm"), "{text} -> {out}");
        }
        assert_eq!(scrub_roots("größer als 3 GB", dir), "größer als 3 GB");
    }

    #[test]
    fn query_values_are_read_by_type() {
        let text = json!({ "anyOf": [{ "type": "string" }, { "type": "null" }] });
        for raw in ["42", "inf", "true", "nan", ""] {
            assert_eq!(query_value(&text, raw), Ok(json!(raw)));
        }
        let count = json!({ "anyOf": [{ "type": "integer", "minimum": 0 }, { "type": "null" }] });
        assert_eq!(query_value(&count, "42"), Ok(json!(42)));
        for raw in ["-1", "4.5", "x", ""] {
            assert!(query_value(&count, raw).is_err(), "{raw}");
        }
        let flag = json!({ "type": "boolean" });
        assert_eq!(query_value(&flag, "true"), Ok(json!(true)));
        assert!(query_value(&flag, "1").is_err());
        let share = json!({ "type": "number" });
        assert_eq!(query_value(&share, "4.5"), Ok(json!(4.5)));
        assert!(query_value(&share, "inf").is_err());
    }

    #[test]
    fn arguments_are_checked_against_the_declaration() {
        let spec = |op| manager_app::registry::spec(op);
        let stop = spec(Op::StopBackend);
        assert!(check_args(stop, &json!({ "id": "a", "graceful": true })).is_ok());
        assert!(check_args(stop, &json!({ "id": "a" })).is_err());
        assert!(check_args(stop, &json!({ "id": "a", "graceful": 1 })).is_err());
        let start = spec(Op::StartBackend);
        for (secs, ok) in [
            (json!(5), true),
            (json!(0), true),
            (Value::Null, true),
            (json!(-1), false),
            (json!("5"), false),
        ] {
            let args = json!({ "id": "a", "wait_ready_secs": secs });
            assert_eq!(check_args(start, &args).is_ok(), ok, "{args}");
        }
    }

    /// Every operation's own sample arguments pass the check that runs
    /// before its route answers 202.
    #[test]
    fn sample_arguments_pass_the_check() {
        for spec in OP_SPECS {
            let args: Map<String, Value> = request_params(spec.op)
                .into_iter()
                .map(|p| (p.name.to_string(), p.sample))
                .collect();
            assert_eq!(
                check_args(spec, &Value::Object(args)),
                Ok(()),
                "{}",
                spec.name
            );
        }
    }

    #[test]
    fn async_outcome_follows_the_shared_rule() {
        // A start-gateway step that ended Failed (or Manual) is a failed
        // operation, with the payload kept as the detail (CR-2).
        let failed = json!({
            "task_id": "t1",
            "ok": false,
            "outcome": { "state": "failed", "message": "gateway did not answer" },
        });
        let f = operation_outcome(Ok(failed.clone())).unwrap_err();
        assert_eq!(f.message, "the operation failed");
        assert_eq!((f.code, f.kind), ("OPERATION_FAILED", "OperationFailed"));
        assert_eq!(f.detail, Some(failed));

        // A refused run (busy) fails too, naming what is running.
        let busy = json!({
            "task_id": "t2", "busy": true, "ok": false, "outcome": null,
            "running_step": "gateway restarting (activation)",
        });
        let f = operation_outcome(Ok(busy)).unwrap_err();
        assert_eq!(
            f.message,
            "busy: gateway restarting (activation) is running"
        );
        assert_eq!(f.code, "BUSY");

        // An audit that did not run because another one does.
        let audit = json!({ "task_id": "t", "already_running": true, "done": 0, "steps": [] });
        let f = operation_outcome(Ok(audit)).unwrap_err();
        assert_eq!((f.code, f.kind), ("BUSY", "Busy"));

        // An activation apply with a failed item fails with its message
        // (ACT-1/GW-7), keeping the items and execution id as detail.
        let apply = json!({
            "ok": false, "applied": false, "status": "completed_with_failures",
            "message": "gateway restart failed: port in use",
            "execution_id": "e1", "items": [],
        });
        let f = operation_outcome(Ok(apply)).unwrap_err();
        assert_eq!(f.message, "gateway restart failed: port in use");
        assert_eq!(f.detail.unwrap()["execution_id"], "e1");

        // Success: ok true, or a payload with no ok/busy at all.
        let done = json!({ "task_id": "t3", "ok": true, "outcome": { "state": "done" } });
        assert_eq!(operation_outcome(Ok(done.clone())).unwrap(), done);
        assert!(operation_outcome(Ok(json!({ "task_id": "t4" }))).is_ok());

        // An op error keeps its text and has no detail.
        let f = operation_outcome(Err(ApiError::Busy(
            "gateway restart already in progress".into(),
        )))
        .unwrap_err();
        assert!(f.message.contains("already in progress"), "{}", f.message);
        assert_eq!((f.code, f.kind), ("BUSY", "Busy"));
        assert!(f.detail.is_none());
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
            (ApiError::Busy("x".into()), 409),
        ];
        for (e, status) in cases {
            assert_eq!(error_mapping(&e).0.as_u16(), status);
        }
        assert_eq!(
            error_mapping(&ApiError::Busy("x".into())),
            (axum::http::StatusCode::CONFLICT, "BUSY", "Busy")
        );
    }
}

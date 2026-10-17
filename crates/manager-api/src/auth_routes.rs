//! Authentication, user-administration, and enrollment endpoints,
//! plus the middleware that guards every route.
//!
//! Authorization is deny-by-default and registry-driven: the
//! middleware resolves each request to its route template and looks
//! the class up in a table built from the same sources as the
//! router itself — `OP_SPECS` classes for operation routes, the
//! plan's classes for derived routes, and explicit entries below
//! for the auth/admin surface. A registered route with no class
//! entry is refused to everyone (it cannot be looked up), and an
//! endpoint can never be exposed below its spec's class because the
//! class is read from the spec, not restated per route.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use manager_app::registry::{role_allows, CapClass, OP_SPECS};

use crate::audit::AuditEntry;
use crate::auth::{AuthError, LoginOutcome, Principal, UserView};
use crate::http::{error_response_raw, DERIVED_ROUTES};
use crate::users::{now_ms, RoleName};
use crate::ApiCtx;

/// Authorization requirement for one route template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthClass {
    /// No credential needed (identity, login, refresh, enrollment).
    Public,
    /// Any validated principal.
    Authenticated,
    /// A registry capability class, checked via `role_allows`.
    Class(CapClass),
}

/// The auth/admin surface: (method, path, class). Recorded in the
/// route table alongside the registry routes for the contract test.
pub const AUTH_ROUTES: &[(&str, &str, AuthClass)] = &[
    ("POST", "/api/v1/auth/login", AuthClass::Public),
    ("POST", "/api/v1/auth/totp", AuthClass::Public),
    ("POST", "/api/v1/auth/refresh", AuthClass::Public),
    ("POST", "/api/v1/enroll", AuthClass::Public),
    ("GET", "/api/v1/auth/me", AuthClass::Authenticated),
    ("POST", "/api/v1/auth/logout", AuthClass::Authenticated),
    ("POST", "/api/v1/auth/password", AuthClass::Authenticated),
    ("GET", "/api/v1/auth/sessions", AuthClass::Authenticated),
    ("DELETE", "/api/v1/auth/sessions/{id}", AuthClass::Authenticated),
    ("POST", "/api/v1/auth/totp/enroll", AuthClass::Authenticated),
    ("POST", "/api/v1/auth/totp/verify", AuthClass::Authenticated),
    ("POST", "/api/v1/auth/totp/disable", AuthClass::Authenticated),
    ("GET", "/api/v1/admin/users", AuthClass::Class(CapClass::UserAdmin)),
    ("POST", "/api/v1/admin/users", AuthClass::Class(CapClass::UserAdmin)),
    ("PATCH", "/api/v1/admin/users/{id}", AuthClass::Class(CapClass::UserAdmin)),
    ("DELETE", "/api/v1/admin/users/{id}", AuthClass::Class(CapClass::UserAdmin)),
    (
        "POST",
        "/api/v1/admin/users/{id}/reset-password",
        AuthClass::Class(CapClass::UserAdmin),
    ),
    (
        "GET",
        "/api/v1/admin/users/{id}/sessions",
        AuthClass::Class(CapClass::UserAdmin),
    ),
    (
        "DELETE",
        "/api/v1/admin/users/{id}/sessions/{session_id}",
        AuthClass::Class(CapClass::UserAdmin),
    ),
    ("GET", "/api/v1/admin/api-tokens", AuthClass::Class(CapClass::UserAdmin)),
    ("POST", "/api/v1/admin/api-tokens", AuthClass::Class(CapClass::UserAdmin)),
    (
        "DELETE",
        "/api/v1/admin/api-tokens/{id}",
        AuthClass::Class(CapClass::UserAdmin),
    ),
    (
        "POST",
        "/api/v1/admin/enrollment-codes",
        AuthClass::Class(CapClass::UserAdmin),
    ),
    (
        "GET",
        "/api/v1/admin/audit",
        AuthClass::Class(CapClass::ApiSettings),
    ),
];

/// Classes for the derived endpoints. Composites inherit the
/// class of the operations they compose, looked up in the
/// registry so a spec change moves the guard with it; the
/// activation apply is the plan's declared exception at L
/// (decision 5). Falls back to R for derived reads.
fn derived_class(method: &str, path: &str) -> AuthClass {
    let op_class = |name: &str| {
        manager_app::registry::op_by_name(name)
            .map(|op| AuthClass::Class(manager_app::registry::spec(op).class))
            .unwrap_or(AuthClass::Class(CapClass::R))
    };
    match (method, path) {
        ("GET", "/api/v1/server") => AuthClass::Public,
        ("GET", "/api/v1/gateway/groups") => AuthClass::Class(CapClass::SConfig),
        ("GET", "/api/v1/updates") => AuthClass::Class(CapClass::SConfig),
        ("POST", "/api/v1/gateway/groups")
        | ("PATCH", "/api/v1/gateway/groups/{name}")
        | ("DELETE", "/api/v1/gateway/groups/{name}") => op_class("update_gateway_config"),
        ("PATCH", "/api/v1/models/{id}") => op_class("model_set_enabled"),
        ("POST", "/api/v1/gateway/start") => op_class("run_wizard_step"),
        ("POST", "/api/v1/activation-plan/apply") => AuthClass::Class(CapClass::L),
        ("GET", "/api/v1/diagnostics/bundles/{bundle_id}/content") => {
            op_class("diagnostics_export_bundle")
        }
        _ => AuthClass::Class(CapClass::R),
    }
}

/// Build the (method, template) -> class table from the registry and
/// the explicit surfaces. Every registered route must appear here;
/// the contract test asserts it.
pub fn route_class_table() -> HashMap<(String, String), AuthClass> {
    let mut table = HashMap::new();
    for spec in OP_SPECS {
        if let Some(r) = spec.route {
            let method = match r.method {
                manager_app::registry::HttpMethod::Get => "GET",
                manager_app::registry::HttpMethod::Post => "POST",
                manager_app::registry::HttpMethod::Put => "PUT",
                manager_app::registry::HttpMethod::Patch => "PATCH",
                manager_app::registry::HttpMethod::Delete => "DELETE",
            };
            table.insert(
                (method.to_string(), r.path.to_string()),
                AuthClass::Class(spec.class),
            );
        }
    }
    for (method, path) in DERIVED_ROUTES {
        table.insert(
            (method.to_string(), path.to_string()),
            derived_class(method, path),
        );
    }
    for (method, path, class) in AUTH_ROUTES {
        table.insert((method.to_string(), path.to_string()), *class);
    }
    table
}

/// Match a concrete path against route templates ({name} segments
/// match any single segment).
fn resolve_template<'a>(
    table: &'a HashMap<(String, String), AuthClass>,
    method: &Method,
    path: &str,
) -> Option<&'a AuthClass> {
    let method = method.as_str();
    let path_segs: Vec<&str> = path.split('/').collect();
    for ((m, template), class) in table {
        if m != method {
            continue;
        }
        let tsegs: Vec<&str> = template.split('/').collect();
        if tsegs.len() != path_segs.len() {
            continue;
        }
        let matches = tsegs.iter().zip(&path_segs).all(|(t, p)| {
            (t.starts_with('{') && t.ends_with('}')) || t == p
        });
        if matches {
            return Some(class);
        }
    }
    None
}

/// The request id assigned by the middleware (also the audit key).
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

/// The peer IP as resolved by the middleware (from the serve
/// layer's ConnectInfo; "unknown" in tests).
#[derive(Debug, Clone)]
pub struct SourceIp(pub String);

#[derive(Clone)]
pub struct AuthMwState {
    pub ctx: Arc<ApiCtx>,
    pub table: Arc<HashMap<(String, String), AuthClass>>,
}

/// Middleware: request id -> principal resolution -> role guard ->
/// handler -> mutation audit. Runs around the whole router.
pub async fn auth_middleware(
    State(mw): State<AuthMwState>,
    mut req: axum::extract::Request,
    next: Next,
) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    req.extensions_mut().insert(RequestId(request_id.clone()));
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let source_ip = req
        .extensions()
        .get::<ConnectInfo<crate::PeerAddr>>()
        .map(|c| c.0.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    req.extensions_mut().insert(SourceIp(source_ip.clone()));

    let class = resolve_template(&mw.table, &method, &path).copied();
    let mut principal: Option<Principal> = None;
    if let Some(class) = class {
        if class != AuthClass::Public {
            let token = req
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_string);
            match token {
                Some(token) => match mw.ctx.auth.validate_bearer(&token).await {
                    Some(p) => {
                        if let AuthClass::Class(c) = class {
                            if !role_allows(p.role, c) {
                                return finish(
                                    error_response_raw(
                                        StatusCode::FORBIDDEN,
                                        "FORBIDDEN",
                                        "Forbidden",
                                        "the authenticated role may not use this endpoint",
                                        None,
                                    ),
                                    &request_id,
                                );
                            }
                        }
                        principal = Some(p);
                    }
                    None => {
                        return finish(
                            error_response_raw(
                                StatusCode::UNAUTHORIZED,
                                "UNAUTHENTICATED",
                                "Unauthenticated",
                                "a valid access token is required",
                                None,
                            ),
                            &request_id,
                        );
                    }
                },
                None => {
                    return finish(
                        error_response_raw(
                            StatusCode::UNAUTHORIZED,
                            "UNAUTHENTICATED",
                            "Unauthenticated",
                            "a valid access token is required",
                            None,
                        ),
                        &request_id,
                    );
                }
            }
        }
    }
    if let Some(p) = &principal {
        req.extensions_mut().insert(p.clone());
    }
    let is_mutation = matches!(
        method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    let resp = next.run(req).await;
    if is_mutation {
        if let Some(p) = &principal {
            mw.ctx.audit.append(&AuditEntry {
                ts_ms: now_ms(),
                actor: p.username.clone(),
                auth_type: p.auth_type.as_str().to_string(),
                source_ip: source_ip.clone(),
                action: format!("{method} {}", template_for_audit(&mw.table, &method, &path)),
                target: audit_target(&path),
                result: resp.status().as_u16().to_string(),
                request_id: request_id.clone(),
            });
        }
    }
    finish(resp, &request_id)
}

fn finish(mut resp: Response, request_id: &str) -> Response {
    if let Ok(v) = axum::http::HeaderValue::from_str(request_id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}

fn template_for_audit(
    table: &HashMap<(String, String), AuthClass>,
    method: &Method,
    path: &str,
) -> String {
    let method = method.as_str();
    for (m, t) in table.keys() {
        if m == method {
            let tsegs: Vec<&str> = t.split('/').collect();
            let psegs: Vec<&str> = path.split('/').collect();
            if tsegs.len() == psegs.len()
                && tsegs
                    .iter()
                    .zip(&psegs)
                    .all(|(a, b)| (a.starts_with('{') && a.ends_with('}')) || a == b)
            {
                return t.clone();
            }
        }
    }
    path.to_string()
}

/// The id-ish last path segment, for the audit target field.
fn audit_target(path: &str) -> Option<String> {
    let last = path.rsplit('/').next()?;
    if last.is_empty() || last == "api" || path.matches('/').count() <= 3 {
        None
    } else {
        Some(last.to_string())
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn source_ip(ip: &SourceIp) -> String {
    ip.0.clone()
}

fn auth_error_response(e: &AuthError) -> Response {
    match e {
        AuthError::InvalidCredentials => error_response_raw(
            StatusCode::UNAUTHORIZED,
            "INVALID_CREDENTIALS",
            "InvalidCredentials",
            "invalid credentials",
            None,
        ),
        AuthError::SessionRevoked => error_response_raw(
            StatusCode::UNAUTHORIZED,
            "SESSION_REVOKED",
            "SessionRevoked",
            "session revoked",
            None,
        ),
        AuthError::InvalidInput(m) => error_response_raw(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            "InvalidInput",
            m,
            None,
        ),
        AuthError::Forbidden(m) => error_response_raw(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "Forbidden",
            m,
            None,
        ),
        AuthError::NotFound(m) => error_response_raw(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "NotFound",
            &format!("not found: {m}"),
            None,
        ),
        AuthError::Internal => error_response_raw(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            "Internal",
            "internal error",
            None,
        ),
    }
}

fn tokens_body(pair: &crate::auth::TokenPair, user: &UserView) -> Value {
    json!({
        "access_token": pair.access_token,
        "refresh_token": pair.refresh_token,
        "token_type": "Bearer",
        "session_id": pair.session_id,
        "access_expires_ms": pair.access_expires_ms,
        "refresh_expires_ms": pair.refresh_expires_ms,
        "user": user,
    })
}

fn audit_auth(
    ctx: &ApiCtx,
    actor: &str,
    auth_type: &str,
    ip: &str,
    action: &str,
    result: &str,
    request_id: &str,
) {
    ctx.audit.append(&AuditEntry {
        ts_ms: now_ms(),
        actor: actor.to_string(),
        auth_type: auth_type.to_string(),
        source_ip: ip.to_string(),
        action: action.to_string(),
        target: None,
        result: result.to_string(),
        request_id: request_id.to_string(),
    });
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginBody {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub device_name: Option<String>,
}

pub async fn handle_login(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(ip): axum::Extension<SourceIp>,
    axum::Json(body): axum::Json<LoginBody>,
) -> Response {
    let ip = source_ip(&ip);
    let rid = uuid::Uuid::new_v4().to_string();
    let device = body.device_name.as_deref().unwrap_or("unknown device");
    match ctx
        .auth
        .login(&body.username, &body.password, device, &ip)
        .await
    {
        Ok(LoginOutcome::Tokens { pair, user }) => {
            audit_auth(&ctx, &user.username, "session", &ip, "POST /api/v1/auth/login", "ok", &rid);
            axum::Json(tokens_body(&pair, &user)).into_response()
        }
        Ok(LoginOutcome::TotpRequired { challenge_id }) => {
            audit_auth(&ctx, &body.username, "session", &ip, "POST /api/v1/auth/login", "totp_required", &rid);
            axum::Json(json!({ "totp_required": true, "challenge_id": challenge_id }))
                .into_response()
        }
        Err(e) => {
            audit_auth(&ctx, &body.username, "session", &ip, "POST /api/v1/auth/login", "invalid_credentials", &rid);
            auth_error_response(&e)
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TotpBody {
    pub challenge_id: String,
    pub code: String,
}

pub async fn handle_totp(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(ip): axum::Extension<SourceIp>,
    axum::Json(body): axum::Json<TotpBody>,
) -> Response {
    let ip = source_ip(&ip);
    match ctx
        .auth
        .complete_totp_login(&body.challenge_id, &body.code, &ip)
        .await
    {
        Ok((pair, user)) => axum::Json(tokens_body(&pair, &user)).into_response(),
        Err(e) => auth_error_response(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshBody {
    pub refresh_token: String,
}

pub async fn handle_refresh(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(ip): axum::Extension<SourceIp>,
    axum::Json(body): axum::Json<RefreshBody>,
) -> Response {
    let ip = source_ip(&ip);
    let rid = uuid::Uuid::new_v4().to_string();
    match ctx.auth.refresh(&body.refresh_token).await {
        Ok((pair, user)) => axum::Json(tokens_body(&pair, &user)).into_response(),
        Err(AuthError::SessionRevoked) => {
            audit_auth(&ctx, "unknown", "session", &ip, "POST /api/v1/auth/refresh", "refresh_reuse_detected", &rid);
            auth_error_response(&AuthError::SessionRevoked)
        }
        Err(e) => auth_error_response(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollBody {
    pub code: String,
    #[serde(default)]
    pub device_name: Option<String>,
}

pub async fn handle_enroll(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(ip): axum::Extension<SourceIp>,
    axum::Json(body): axum::Json<EnrollBody>,
) -> Response {
    let ip = source_ip(&ip);
    let rid = uuid::Uuid::new_v4().to_string();
    let device = body.device_name.as_deref().unwrap_or("unknown device");
    match ctx.auth.redeem_enrollment_code(&body.code, device).await {
        Ok((pair, user)) => {
            audit_auth(&ctx, &user.username, "enrollment", &ip, "POST /api/v1/enroll", "ok", &rid);
            (StatusCode::CREATED, axum::Json(tokens_body(&pair, &user))).into_response()
        }
        Err(e) => {
            audit_auth(&ctx, "unknown", "enrollment", &ip, "POST /api/v1/enroll", "invalid_credentials", &rid);
            auth_error_response(&e)
        }
    }
}

pub async fn handle_me(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(p): axum::Extension<Principal>,
) -> Response {
    let view = ctx
        .users
        .read(|f| f.users.iter().find(|u| u.id == p.user_id).map(UserView::from))
        .await;
    match view {
        Some(user) => axum::Json(json!({
            "user": user,
            "auth_type": p.auth_type.as_str(),
            "session_id": p.session_id,
        }))
        .into_response(),
        None => auth_error_response(&AuthError::InvalidCredentials),
    }
}

pub async fn handle_logout(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(p): axum::Extension<Principal>,
) -> Response {
    if p.auth_type == crate::auth::AuthType::ApiToken {
        return error_response_raw(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            "InvalidInput",
            "API tokens are revoked by an administrator, not logged out",
            None,
        );
    }
    let _ = ctx.auth.revoke_session(&p.session_id).await;
    axum::Json(json!({ "ok": true })).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordBody {
    pub current_password: String,
    pub new_password: String,
}

pub async fn handle_change_password(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(p): axum::Extension<Principal>,
    axum::Json(body): axum::Json<PasswordBody>,
) -> Response {
    let keep = if p.auth_type == crate::auth::AuthType::Session {
        Some(p.session_id.as_str())
    } else {
        None
    };
    match ctx
        .auth
        .change_password(&p.user_id, &body.current_password, &body.new_password, keep)
        .await
    {
        Ok(()) => axum::Json(json!({ "ok": true })).into_response(),
        Err(e) => auth_error_response(&e),
    }
}

pub async fn handle_list_sessions(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(p): axum::Extension<Principal>,
) -> Response {
    let sessions = ctx
        .users
        .read(|f| {
            f.sessions
                .iter()
                .filter(|s| s.user_id == p.user_id && !s.revoked)
                .map(|s| {
                    json!({
                        "id": s.id,
                        "device_name": s.device_name,
                        "created_ms": s.created_ms,
                        "last_used_ms": s.last_used_ms,
                        "access_expires_ms": s.access_expires_ms,
                        "refresh_expires_ms": s.refresh_expires_ms,
                        "current": s.id == p.session_id,
                    })
                })
                .collect::<Vec<_>>()
        })
        .await;
    axum::Json(json!({ "sessions": sessions })).into_response()
}

pub async fn handle_revoke_session(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(p): axum::Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    let owns = ctx
        .users
        .read(|f| {
            f.sessions
                .iter()
                .any(|s| s.id == id && s.user_id == p.user_id)
        })
        .await;
    if !owns {
        return auth_error_response(&AuthError::NotFound("session".into()));
    }
    let _ = ctx.auth.revoke_session(&id).await;
    axum::Json(json!({ "ok": true })).into_response()
}

pub async fn handle_totp_enroll(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(p): axum::Extension<Principal>,
) -> Response {
    match ctx.auth.totp_begin_enrollment(&p.user_id, &p.username).await {
        Ok((secret, uri)) => {
            axum::Json(json!({ "secret": secret, "otpauth_uri": uri })).into_response()
        }
        Err(e) => auth_error_response(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TotpVerifyBody {
    pub code: String,
}

pub async fn handle_totp_verify(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(p): axum::Extension<Principal>,
    axum::Json(body): axum::Json<TotpVerifyBody>,
) -> Response {
    match ctx.auth.totp_verify_enrollment(&p.user_id, &body.code).await {
        Ok(codes) => axum::Json(json!({ "recovery_codes": codes })).into_response(),
        Err(e) => auth_error_response(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TotpDisableBody {
    pub password: String,
}

pub async fn handle_totp_disable(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(p): axum::Extension<Principal>,
    axum::Json(body): axum::Json<TotpDisableBody>,
) -> Response {
    let ok = ctx
        .users
        .read(|f| {
            f.users
                .iter()
                .find(|u| u.id == p.user_id)
                .map(|u| crate::users::verify_password(&body.password, &u.password_hash))
                .unwrap_or(false)
        })
        .await;
    if !ok {
        return auth_error_response(&AuthError::InvalidCredentials);
    }
    match ctx.auth.totp_disable(&p.user_id).await {
        Ok(()) => axum::Json(json!({ "ok": true })).into_response(),
        Err(e) => auth_error_response(&e),
    }
}

// -- admin: users -----------------------------------------------------------

pub async fn handle_admin_list_users(State(ctx): State<Arc<ApiCtx>>) -> Response {
    let users = ctx
        .users
        .read(|f| f.users.iter().map(UserView::from).collect::<Vec<_>>())
        .await;
    axum::Json(json!({ "users": users })).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateUserBody {
    pub username: String,
    pub password: String,
    pub role: String,
}

pub async fn handle_admin_create_user(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Json(body): axum::Json<CreateUserBody>,
) -> Response {
    let Some(role) = RoleName::parse(&body.role) else {
        return auth_error_response(&AuthError::InvalidInput(format!(
            "unknown role `{}`",
            body.role
        )));
    };
    match ctx
        .auth
        .create_user(&body.username, &body.password, role)
        .await
    {
        Ok(view) => (StatusCode::CREATED, axum::Json(json!({ "user": view }))).into_response(),
        Err(e) => auth_error_response(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateUserBody {
    pub role: Option<String>,
    pub disabled: Option<bool>,
}

pub async fn handle_admin_update_user(
    State(ctx): State<Arc<ApiCtx>>,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<UpdateUserBody>,
) -> Response {
    let role = match &body.role {
        Some(r) => match RoleName::parse(r) {
            Some(parsed) => Some(parsed),
            None => {
                return auth_error_response(&AuthError::InvalidInput(format!(
                    "unknown role `{r}`"
                )))
            }
        },
        None => None,
    };
    match ctx.auth.update_user(&id, role, body.disabled).await {
        Ok(view) => axum::Json(json!({ "user": view })).into_response(),
        Err(e) => auth_error_response(&e),
    }
}

pub async fn handle_admin_delete_user(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(p): axum::Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    match ctx.auth.delete_user(&p.user_id, &id).await {
        Ok(()) => axum::Json(json!({ "ok": true })).into_response(),
        Err(e) => auth_error_response(&e),
    }
}

pub async fn handle_admin_reset_password(
    State(ctx): State<Arc<ApiCtx>>,
    Path(id): Path<String>,
) -> Response {
    match ctx.auth.admin_reset_password(&id).await {
        Ok(temp) => {
            axum::Json(json!({ "temporary_password": temp, "must_change_password": true }))
                .into_response()
        }
        Err(e) => auth_error_response(&e),
    }
}

pub async fn handle_admin_list_user_sessions(
    State(ctx): State<Arc<ApiCtx>>,
    Path(id): Path<String>,
) -> Response {
    let sessions = ctx
        .users
        .read(|f| {
            f.sessions
                .iter()
                .filter(|s| s.user_id == id && !s.revoked)
                .map(|s| {
                    json!({
                        "id": s.id,
                        "device_name": s.device_name,
                        "created_ms": s.created_ms,
                        "last_used_ms": s.last_used_ms,
                    })
                })
                .collect::<Vec<_>>()
        })
        .await;
    axum::Json(json!({ "sessions": sessions })).into_response()
}

pub async fn handle_admin_revoke_user_session(
    State(ctx): State<Arc<ApiCtx>>,
    Path((user_id, session_id)): Path<(String, String)>,
) -> Response {
    let owns = ctx
        .users
        .read(|f| {
            f.sessions
                .iter()
                .any(|s| s.id == session_id && s.user_id == user_id)
        })
        .await;
    if !owns {
        return auth_error_response(&AuthError::NotFound("session".into()));
    }
    let _ = ctx.auth.revoke_session(&session_id).await;
    axum::Json(json!({ "ok": true })).into_response()
}

// -- admin: API tokens --------------------------------------------------------

pub async fn handle_admin_list_tokens(State(ctx): State<Arc<ApiCtx>>) -> Response {
    let tokens = ctx
        .users
        .read(|f| {
            f.api_tokens
                .iter()
                .map(|t| {
                    json!({
                        "id": t.id,
                        "user_id": t.user_id,
                        "name": t.name,
                        "role": t.role.as_str(),
                        "issued_by": t.issued_by,
                        "created_ms": t.created_ms,
                        "expires_ms": t.expires_ms,
                        "last_used_ms": t.last_used_ms,
                        "revoked": t.revoked,
                    })
                })
                .collect::<Vec<_>>()
        })
        .await;
    axum::Json(json!({ "api_tokens": tokens })).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTokenBody {
    pub user_id: String,
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub expires_days: Option<i64>,
}

pub async fn handle_admin_create_token(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Extension(p): axum::Extension<Principal>,
    axum::Json(body): axum::Json<CreateTokenBody>,
) -> Response {
    let Some(role) = RoleName::parse(&body.role) else {
        return auth_error_response(&AuthError::InvalidInput(format!(
            "unknown role `{}`",
            body.role
        )));
    };
    match ctx
        .auth
        .create_api_token(&p, &body.user_id, &body.name, role, body.expires_days)
        .await
    {
        Ok((meta, value)) => (
            StatusCode::CREATED,
            axum::Json(json!({
                "api_token": {
                    "id": meta.id,
                    "user_id": meta.user_id,
                    "name": meta.name,
                    "role": meta.role.as_str(),
                    "created_ms": meta.created_ms,
                    "expires_ms": meta.expires_ms,
                },
                "value": value,
            })),
        )
            .into_response(),
        Err(e) => auth_error_response(&e),
    }
}

pub async fn handle_admin_revoke_token(
    State(ctx): State<Arc<ApiCtx>>,
    Path(id): Path<String>,
) -> Response {
    match ctx.auth.revoke_api_token(&id).await {
        Ok(()) => axum::Json(json!({ "ok": true })).into_response(),
        Err(e) => auth_error_response(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateEnrollmentBody {
    pub user_id: String,
}

/// GET /api/v1/admin/audit?offset=&limit= — the API audit log,
/// newest first (admin only; reads are unaudited except
/// S-disclosure per the plan, and this is an ApiSettings route).
pub async fn handle_admin_audit(
    State(ctx): State<Arc<ApiCtx>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let offset: usize = q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
    let limit: usize = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
        .min(1000);
    let entries = ctx.audit.read_recent(offset, limit);
    axum::Json(json!({ "entries": entries, "offset": offset, "limit": limit }))
        .into_response()
}

pub async fn handle_admin_create_enrollment(
    State(ctx): State<Arc<ApiCtx>>,
    axum::Json(body): axum::Json<CreateEnrollmentBody>,
) -> Response {
    match ctx.auth.create_enrollment_code(&body.user_id).await {
        Ok(code) => (
            StatusCode::CREATED,
            axum::Json(json!({
                "code": code,
                "expires_ms": now_ms() + 10 * 60 * 1000,
            })),
        )
            .into_response(),
        Err(e) => auth_error_response(&e),
    }
}

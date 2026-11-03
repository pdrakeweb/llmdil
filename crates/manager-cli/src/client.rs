//! HTTP client for the LLMdil Admin API (v1, audited Stage 1).
//!
//! Transport: HTTPS with certificate-fingerprint pinning (see
//! [`pinning`]). Auth: `Authorization: Bearer <access_token>`.
//! Errors use the server's envelope
//! (`{"error": {"code", "kind", "message", "details"}}`); the client
//! branches on `code`, never on `message`.
//!
//! Token handling: tokens live in memory only. The client never
//! writes a token to disk, a config file, or a log. On a 401 the
//! client rotates the refresh token once (when one is held) and
//! retries the request once; a rotated refresh token presented
//! again revokes the session family server-side, so the client
//! never retries a refresh twice.

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::pinning::{parse_fingerprint, pinned_client_config};

/// Operation record, per API-DESIGN §1: id, kind, status
/// (running/done/failed/aborted), detail, created/started/finished
/// epoch ms, error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub kind: String,
    pub status: String,
    #[allow(dead_code)]
    pub detail: Option<Value>,
    #[allow(dead_code)]
    pub created_ms: Option<i64>,
    #[allow(dead_code)]
    pub started_ms: Option<i64>,
    #[allow(dead_code)]
    pub finished_ms: Option<i64>,
    pub error: Option<Value>,
}

impl Operation {
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "done" | "failed" | "aborted")
    }
}

/// Token pair. Debug is redacted: tokens must never appear in logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default = "default_token_type")]
    pub token_type: String,
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

impl TokenPair {
    /// Redacted debug: field names only, never values.
    pub fn debug_redacted(&self) -> String {
        format!(
            "TokenPair {{ access_token: <redacted>, refresh_token: <redacted>, token_type: {} }}",
            self.token_type
        )
    }
}

/// The server's error envelope body.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorBody {
    pub code: String,
    pub kind: String,
    pub message: String,
    pub details: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("API error {code} ({kind}): {message}", code = .0.code, kind = .0.kind, message = .0.message)]
    Api(ApiErrorBody),
    #[error("HTTP {0} with an unparseable body")]
    UnparsedStatus(StatusCode),
    #[error("no access token: set LLMDIL_API_TOKEN or pass --token-stdin")]
    NoToken,
    #[error("operation {0} ended {1}")]
    OperationFailed(String, String),
    #[error("{0}")]
    Input(String),
}

impl ApiError {
    /// 409 BUSY means "already running — here is the operation",
    /// not an error (API-DESIGN §1).
    pub fn is_busy(&self) -> bool {
        matches!(self, ApiError::Api(b) if b.code == "BUSY")
    }
}

/// Poll cadence from the API contract (API-DESIGN §1): 1 s for the
/// first 10 s, then 5 s.
pub fn next_poll_delay(elapsed: Duration) -> Duration {
    if elapsed.as_secs() < 10 {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(5)
    }
}

pub struct ApiClient {
    http: Client,
    base: String,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

impl ApiClient {
    /// Build a client pinned to `fingerprint_hex` (64 hex chars, as
    /// shown by the LLMdil UI's "Certificate SHA-256" row). `base`
    /// is the API base URL without a trailing slash, e.g.
    /// `https://192.168.5.23:47821`.
    pub fn new(base: &str, fingerprint_hex: &str) -> Result<Self, ApiError> {
        let fp = parse_fingerprint(fingerprint_hex)
            .map_err(ApiError::Input)?;
        let config = pinned_client_config(fp);
        let http = Client::builder()
            .use_preconfigured_tls(config)
            .build()
            .map_err(ApiError::Transport)?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            access_token: None,
            refresh_token: None,
        })
    }

    pub fn set_tokens(&mut self, pair: TokenPair) {
        self.access_token = Some(pair.access_token);
        self.refresh_token = Some(pair.refresh_token);
    }

    pub fn set_access_token(&mut self, token: String) {
        self.access_token = Some(token);
    }

    pub fn set_refresh_token(&mut self, token: String) {
        self.refresh_token = Some(token);
    }

    pub fn has_token(&self) -> bool {
        self.access_token.is_some()
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Rotate the refresh token once. Returns the new pair, or None
    /// when no refresh token is held or the rotation failed. A failed
    /// rotation clears the held tokens: presenting a rotated token
    /// again revokes the session family server-side.
    async fn try_refresh(&mut self) -> Option<TokenPair> {
        let refresh = self.refresh_token.clone()?;
        // NOTE: the workspace reqwest build has no `json` feature, so
        // bodies are serialized and parsed by hand (same bytes).
        let body = serde_json::to_string(&serde_json::json!({
            "refresh_token": refresh
        }))
        .ok()?;
        let resp = self
            .http
            .post(self.url("/api/v1/auth/refresh"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            self.access_token = None;
            self.refresh_token = None;
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
        serde_json::from_slice::<TokenPair>(&bytes).ok()
    }

    fn build_request(
        &self,
        method: &reqwest::Method,
        path: &str,
        body: &Option<Value>,
        authenticated: bool,
    ) -> Result<reqwest::RequestBuilder, ApiError> {
        let mut req = self.http.request(method.clone(), self.url(path));
        if authenticated {
            let token = self.access_token.clone().ok_or(ApiError::NoToken)?;
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(body) = body {
            // See try_refresh: no reqwest `json` feature in this
            // workspace; serialize by hand.
            let text = serde_json::to_string(body).map_err(|e| {
                ApiError::Input(format!("could not serialize request body: {e}"))
            })?;
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(text);
        }
        Ok(req)
    }

    /// Send one request; on 401 with a refresh token held, rotate
    /// once and resend the same request (same method, path, body)
    /// with the new token, then return that result. No second
    /// refresh is attempted.
    async fn request_json(
        &mut self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        authenticated: bool,
    ) -> Result<Value, ApiError> {
        let (status, value) = self
            .execute_once(self.build_request(&method, path, &body, authenticated)?)
            .await?;
        if status == StatusCode::UNAUTHORIZED && self.refresh_token.is_some() {
            if let Some(pair) = self.try_refresh().await {
                self.set_tokens(pair);
                let (status, value) = self
                    .execute_once(self.build_request(
                        &method,
                        path,
                        &body,
                        authenticated,
                    )?)
                    .await?;
                return Self::check(status, value);
            }
        }
        Self::check(status, value)
    }

    async fn execute_once(
        &mut self,
        req: reqwest::RequestBuilder,
    ) -> Result<(StatusCode, Value), ApiError> {
        let resp = req.send().await.map_err(ApiError::Transport)?;
        let status = resp.status();
        let bytes = resp.bytes().await.unwrap_or_default();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok((status, body))
    }

    fn check(status: StatusCode, value: Value) -> Result<Value, ApiError> {
        if status.is_success() {
            return Ok(value);
        }
        if let Some(err) = value.get("error") {
            if let Ok(body) = serde_json::from_value::<ApiErrorBody>(err.clone()) {
                return Err(ApiError::Api(body));
            }
        }
        Err(ApiError::UnparsedStatus(status))
    }

    /// Public identity document (no credential required).
    pub async fn server(&mut self) -> Result<Value, ApiError> {
        self.request_json(reqwest::Method::GET, "/api/v1/server", None, false)
            .await
    }

    pub async fn get(&mut self, path: &str) -> Result<Value, ApiError> {
        self.request_json(reqwest::Method::GET, path, None, true)
            .await
    }

    pub async fn post(
        &mut self,
        path: &str,
        body: Value,
    ) -> Result<Value, ApiError> {
        self.request_json(reqwest::Method::POST, path, Some(body), true)
            .await
    }

    pub async fn delete(&mut self, path: &str) -> Result<Value, ApiError> {
        self.request_json(reqwest::Method::DELETE, path, None, true)
            .await
    }

    /// POST that returns a 202 operation record. The caller polls it.
    pub async fn post_operation(
        &mut self,
        path: &str,
        body: Value,
    ) -> Result<Operation, ApiError> {
        let value = self.post(path, body).await?;
        serde_json::from_value::<Operation>(value).map_err(|e| {
            ApiError::Input(format!("operation record did not parse: {e}"))
        })
    }

    /// Poll an operation to a terminal state, on the API cadence
    /// (1 s for the first 10 s, then 5 s).
    pub async fn poll_operation(
        &mut self,
        op_id: &str,
        on_tick: &mut dyn FnMut(&Operation),
    ) -> Result<Operation, ApiError> {
        let started = std::time::Instant::now();
        loop {
            let value = self.get(&format!("/api/v1/operations/{op_id}")).await?;
            let op: Operation = serde_json::from_value::<Operation>(value)
                .map_err(|e| {
                    ApiError::Input(format!("operation record did not parse: {e}"))
                })?;
            on_tick(&op);
            if op.is_terminal() {
                return Ok(op);
            }
            tokio::time::sleep(next_poll_delay(started.elapsed())).await;
        }
    }

    // ---- Auth (public routes; no Bearer header) ----

    pub async fn login(
        &mut self,
        username: &str,
        password: &str,
        device_name: Option<&str>,
    ) -> Result<LoginOutcome, ApiError> {
        let mut body = serde_json::json!({
            "username": username,
            "password": password,
        });
        if let Some(device) = device_name {
            body["device_name"] = serde_json::json!(device);
        }
        let value = self
            .request_json(
                reqwest::Method::POST,
                "/api/v1/auth/login",
                Some(body),
                false,
            )
            .await?;
        if value.get("totp_required").and_then(Value::as_bool) == Some(true) {
            let challenge_id = value
                .get("challenge_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return Ok(LoginOutcome::TotpRequired { challenge_id });
        }
        let pair: TokenPair = serde_json::from_value(value).map_err(|e| {
            ApiError::Input(format!("login response did not parse: {e}"))
        })?;
        Ok(LoginOutcome::Pair(pair))
    }

    pub async fn totp(
        &mut self,
        challenge_id: &str,
        code: &str,
    ) -> Result<TokenPair, ApiError> {
        let value = self
            .request_json(
                reqwest::Method::POST,
                "/api/v1/auth/totp",
                Some(serde_json::json!({
                    "challenge_id": challenge_id,
                    "code": code,
                })),
                false,
            )
            .await?;
        serde_json::from_value(value).map_err(|e| {
            ApiError::Input(format!("totp response did not parse: {e}"))
        })
    }

    pub async fn enroll(
        &mut self,
        code: &str,
        device_name: Option<&str>,
    ) -> Result<TokenPair, ApiError> {
        let mut body = serde_json::json!({ "code": code });
        if let Some(device) = device_name {
            body["device_name"] = serde_json::json!(device);
        }
        let value = self
            .request_json(
                reqwest::Method::POST,
                "/api/v1/enroll",
                Some(body),
                false,
            )
            .await?;
        serde_json::from_value(value).map_err(|e| {
            ApiError::Input(format!("enroll response did not parse: {e}"))
        })
    }

    pub async fn refresh(&mut self, refresh_token: &str) -> Result<TokenPair, ApiError> {
        let value = self
            .request_json(
                reqwest::Method::POST,
                "/api/v1/auth/refresh",
                Some(serde_json::json!({ "refresh_token": refresh_token })),
                false,
            )
            .await?;
        serde_json::from_value(value).map_err(|e| {
            ApiError::Input(format!("refresh response did not parse: {e}"))
        })
    }
}

/// Outcome of `POST /api/v1/auth/login`: either a token pair, or a
/// TOTP challenge the caller must complete.
#[derive(Debug)]
pub enum LoginOutcome {
    Pair(TokenPair),
    TotpRequired { challenge_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_delay_is_1s_then_5s() {
        assert_eq!(
            next_poll_delay(Duration::from_secs(0)),
            Duration::from_secs(1)
        );
        assert_eq!(
            next_poll_delay(Duration::from_secs(9)),
            Duration::from_secs(1)
        );
        assert_eq!(
            next_poll_delay(Duration::from_secs(10)),
            Duration::from_secs(5)
        );
        assert_eq!(
            next_poll_delay(Duration::from_secs(600)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn operation_terminal_states() {
        let op = Operation {
            id: "x".into(),
            kind: "backend_start".into(),
            status: "running".into(),
            detail: None,
            created_ms: None,
            started_ms: None,
            finished_ms: None,
            error: None,
        };
        assert!(!op.is_terminal());
        let mut done = op.clone();
        done.status = "done".into();
        assert!(done.is_terminal());
        let mut failed = op.clone();
        failed.status = "failed".into();
        assert!(failed.is_terminal());
        let mut aborted = op;
        aborted.status = "aborted".into();
        assert!(aborted.is_terminal());
    }

    #[test]
    fn token_pair_debug_does_not_leak_values() {
        let pair = TokenPair {
            access_token: "secret-access".into(),
            refresh_token: "secret-refresh".into(),
            token_type: "Bearer".into(),
        };
        let redacted = pair.debug_redacted();
        assert!(!redacted.contains("secret-access"));
        assert!(!redacted.contains("secret-refresh"));
        assert!(redacted.contains("Bearer"));
    }
}

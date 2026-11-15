//! Authentication and identity services (auth plan): password
//! login with brute-force lockout, per-device sessions with
//! rotating refresh tokens and reuse detection, admin-issued API
//! tokens, optional per-user TOTP, and one-time enrollment codes.
//!
//! Every credential check that can fail for an unknown user, a
//! wrong password, a disabled account, or a locked account returns
//! the same `InvalidCredentials` error and (for passwords) pays the
//! same argon2 cost, so responses do not enumerate users.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use manager_wizard::SystemOps;

use crate::users::{
    generate_token, hash_password, hash_token, now_ms, password_policy_error,
    verify_password_async, ApiToken, RoleName, Session, User, UserStore, ACCESS_TTL_MS,
    REFRESH_TTL_MS,
};
use manager_app::registry::Role;

mod admin;
mod api_tokens;
mod enrollment;
mod lockout;
mod login;
mod passwords;
mod sessions;
mod totp;

#[cfg(test)]
mod tests;

pub use totp::{base32_decode, base32_encode, totp_at, totp_matching_steps, totp_verify};

// ---------------------------------------------------------------------------
// Principals and errors
// ---------------------------------------------------------------------------

/// How a principal authenticated. Recorded in the audit log, and gates account
/// self-service: API tokens may not change passwords or TOTP (SEC-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    Session,
    ApiToken,
}

impl AuthType {
    /// Stable wire and audit spelling (`session`, `api_token`).
    pub fn as_str(self) -> &'static str {
        match self {
            AuthType::Session => "session",
            AuthType::ApiToken => "api_token",
        }
    }
}

/// Who a validated credential belongs to.
#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: String,
    pub username: String,
    pub role: Role,
    pub auth_type: AuthType,
    /// Session id, or the API token id for token principals.
    pub session_id: String,
    /// The account's must-change-password flag, read from the user
    /// store at validation time (SEC-03). While set, the middleware
    /// admits only change-password, logout and `/auth/me`.
    pub must_change_password: bool,
}

/// Failures from [`AuthService`], mapped one-to-one onto HTTP error envelopes by the auth
/// routes. `Internal` deliberately carries no detail, so store errors never reach clients.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// Unknown user, wrong password, disabled account, or locked
    /// account — deliberately indistinguishable.
    #[error("invalid credentials")]
    InvalidCredentials,
    /// A rotated-out refresh token was presented again.
    #[error("session revoked")]
    SessionRevoked,
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// The account carries the must-change-password flag (set by an
    /// admin reset); only change-password, logout and `/auth/me` are
    /// open to it (SEC-03, auth plan §3.6).
    #[error("password change required")]
    PasswordChangeRequired,
    #[error("internal error")]
    Internal,
}

impl From<crate::users::StoreError> for AuthError {
    fn from(_: crate::users::StoreError) -> Self {
        AuthError::Internal
    }
}

/// A freshly issued access/refresh pair. The token strings exist only here — the store
/// keeps their SHA-256 — so this is the one chance to hand them to the client.
#[derive(Debug, Clone)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub session_id: String,
    pub access_expires_ms: i64,
    pub refresh_expires_ms: i64,
}

/// Result of the password stage of [`AuthService::login`]: tokens directly or, for a
/// TOTP-enabled account, a single-use challenge for [`AuthService::complete_totp_login`].
#[derive(Debug, Clone)]
pub enum LoginOutcome {
    Tokens { pair: TokenPair, user: UserView },
    TotpRequired { challenge_id: String },
}

/// The client-safe projection of a [`User`]: no password hash, recovery-code hashes or
/// other secret material. Response bodies use this, never `User`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UserView {
    pub id: String,
    pub username: String,
    pub role: String,
    pub disabled: bool,
    pub must_change_password: bool,
    pub totp_enabled: bool,
    pub created_ms: i64,
}

impl From<&User> for UserView {
    fn from(u: &User) -> Self {
        Self {
            id: u.id.clone(),
            username: u.username.clone(),
            role: u.role.as_str().to_string(),
            disabled: u.disabled,
            must_change_password: u.must_change_password,
            totp_enabled: u.totp_enabled,
            created_ms: u.created_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// Credential store for TOTP secrets
// ---------------------------------------------------------------------------

/// Custody for TOTP secrets: the OS credential store in production
/// (via the wizard's SystemOps, which reach Credential Manager through
/// the [`crate::secrets::SecretStore`] seam), memory in tests. This
/// async, text-valued adapter keeps the wizard's UTF-16LE credential
/// encoding; byte-valued callers use `SecretStore` directly. Secrets
/// never touch `admin-users.json`.
#[async_trait::async_trait]
pub trait CredentialStore: Send + Sync {
    /// The secret stored under `target`, or `None` when there is none. Never log it.
    async fn read(&self, target: &str) -> Option<String>;
    /// Store `secret` under `target`, replacing any existing value.
    async fn write(&self, target: &str, secret: &str) -> Result<(), String>;
    /// Remove the secret under `target`. Best-effort and infallible: callers disable the
    /// factor in the user store first, so a leftover secret is never consulted.
    async fn delete(&self, target: &str);
}

/// Production store backed by the OS credential manager.
pub struct WizardCredentialStore;

#[async_trait::async_trait]
impl CredentialStore for WizardCredentialStore {
    async fn read(&self, target: &str) -> Option<String> {
        manager_wizard::ops::RealSystemOps::default()
            .credential_read(target)
            .await
    }

    async fn write(&self, target: &str, secret: &str) -> Result<(), String> {
        manager_wizard::ops::RealSystemOps::default()
            .credential_write(target, secret)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete(&self, target: &str) {
        // The wizard ops layer has no delete; clearing the value is
        // the delete (an empty credential reads as absent).
        let _ = self.write(target, "").await;
    }
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryCredentialStore {
    map: Mutex<HashMap<String, String>>,
}

#[async_trait::async_trait]
impl CredentialStore for MemoryCredentialStore {
    async fn read(&self, target: &str) -> Option<String> {
        self.map.lock().await.get(target).cloned()
    }
    async fn write(&self, target: &str, secret: &str) -> Result<(), String> {
        self.map
            .lock()
            .await
            .insert(target.to_string(), secret.to_string());
        Ok(())
    }
    async fn delete(&self, target: &str) {
        self.map.lock().await.remove(target);
    }
}

fn totp_target(user_id: &str) -> String {
    format!("llmdil/totp/{user_id}")
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

/// Longest username `create_user` accepts. Login refuses longer
/// names before any per-account bookkeeping, so an unauthenticated
/// client cannot grow the lockout map or the audit log with
/// megabyte-sized usernames (finding SEC-05).
pub const MAX_USERNAME_LEN: usize = 64;
const LOCK_THRESHOLD: usize = 5;
const LOCK_WINDOW_MS: i64 = 15 * 60 * 1000;
const LOCK_BASE_MS: i64 = 15 * 60 * 1000;
const LOCK_MAX_MS: i64 = 60 * 60 * 1000;
const IP_FAILURE_LIMIT: usize = 30;
const CHALLENGE_TTL_MS: i64 = 5 * 60 * 1000;
const ENROLLMENT_TTL_MS: i64 = 10 * 60 * 1000;
/// Lifetime of both tokens of a session minted for an account that
/// must change its password (SEC-03): long enough to change it, never
/// the 30-day refresh lifetime of a normal session.
pub const MUST_CHANGE_SESSION_TTL_MS: i64 = 15 * 60 * 1000;
/// Minimum interval between persisted last-used updates for one
/// session or API token (SEC-14: no store rewrite per request).
const LAST_USED_WRITE_INTERVAL_MS: i64 = 60_000;

#[derive(Default)]
struct LockState {
    failures: Vec<i64>,
    locked_until_ms: i64,
    lock_count: u32,
}

struct TotpChallenge {
    user_id: String,
    device_name: String,
    expires_ms: i64,
}

struct Enrollment {
    user_id: String,
    expires_ms: i64,
}

/// A TOTP secret from `totp_begin_enrollment` that has not been
/// confirmed yet (code-review B-6). It lives only in this in-memory
/// map — never in the credential store — so an unconfirmed
/// re-enrollment cannot overwrite or invalidate the live factor,
/// and it expires [`PENDING_TOTP_TTL_MS`] after it was issued.
struct PendingTotp {
    secret: String,
    expires_ms: i64,
}

/// How long an unconfirmed TOTP enrollment stays valid.
const PENDING_TOTP_TTL_MS: i64 = 10 * 60 * 1000;

/// Credential checks and account lifecycle over a [`UserStore`]. Lockout counters, TOTP
/// login challenges, enrollment codes and pending TOTP secrets live in memory per
/// instance (lost on restart), so a process must share ONE instance — see
/// [`crate::ApiCtx::with_auth`].
pub struct AuthService {
    store: Arc<UserStore>,
    creds: Arc<dyn CredentialStore>,
    lockouts: Mutex<HashMap<String, LockState>>,
    ip_failures: Mutex<HashMap<String, Vec<i64>>>,
    challenges: Mutex<HashMap<String, TotpChallenge>>,
    enrollments: Mutex<HashMap<String, Enrollment>>,
    pending_totp: Mutex<HashMap<String, PendingTotp>>,
    /// Where lockout events are recorded (SEC-13). Attached by
    /// [`crate::ApiCtx::with_auth`]; `None` (tests, desktop before the
    /// server first starts) records nothing.
    audit: std::sync::Mutex<Option<Arc<crate::audit::AuditLog>>>,
}

impl AuthService {
    /// A service with empty in-memory state, returned in an `Arc` because the HTTP handlers
    /// and the desktop shell share it.
    pub fn new(store: Arc<UserStore>, creds: Arc<dyn CredentialStore>) -> Arc<Self> {
        Arc::new(Self {
            store,
            creds,
            lockouts: Mutex::new(HashMap::new()),
            ip_failures: Mutex::new(HashMap::new()),
            challenges: Mutex::new(HashMap::new()),
            enrollments: Mutex::new(HashMap::new()),
            pending_totp: Mutex::new(HashMap::new()),
            audit: std::sync::Mutex::new(None),
        })
    }

    /// Record lockout events in `log` from now on (SEC-13). Called by
    /// [`crate::ApiCtx::with_auth`]; the latest attachment wins.
    pub fn attach_audit(&self, log: Arc<crate::audit::AuditLog>) {
        *self.audit.lock().unwrap_or_else(|e| e.into_inner()) = Some(log);
    }

    /// Audit an account lock engaging. Never fails the caller.
    fn audit_lock(&self, username_key: &str, source_ip: &str, locked_for_ms: i64) {
        let log = self.audit.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(log) = log else {
            return;
        };
        // Bounded by MAX_USERNAME_LEN on the login path already; cap
        // again so no caller can grow the log (SEC-05).
        let actor: String = username_key.chars().take(MAX_USERNAME_LEN).collect();
        log.append(&crate::audit::AuditEntry {
            ts_ms: now_ms(),
            actor: actor.clone(),
            auth_type: "session".to_string(),
            source_ip: source_ip.to_string(),
            action: "account_locked".to_string(),
            target: Some(actor),
            result: format!("locked_for_{}s", locked_for_ms / 1000),
            request_id: uuid::Uuid::new_v4().to_string(),
        });
    }

    /// The username a pending TOTP login challenge belongs to, without
    /// consuming it, so the second login step can be audited under
    /// the right actor (SEC-13). `None` for an unknown challenge.
    pub async fn totp_challenge_username(&self, challenge_id: &str) -> Option<String> {
        let user_id = self
            .challenges
            .lock()
            .await
            .get(challenge_id)
            .map(|c| c.user_id.clone())?;
        self.store
            .read(|f| {
                f.users
                    .iter()
                    .find(|u| u.id == user_id)
                    .map(|u| u.username.clone())
            })
            .await
    }

    /// The backing user store; `ApiCtx` reads users and sessions through the same instance.
    pub fn store(&self) -> &Arc<UserStore> {
        &self.store
    }
}

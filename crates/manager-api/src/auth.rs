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
    generate_token, hash_password, hash_token, now_ms, password_policy_error, verify_password,
    ApiToken, RoleName, Session, User, UserStore, ACCESS_TTL_MS, REFRESH_TTL_MS,
};
use manager_app::registry::Role;

// ---------------------------------------------------------------------------
// Principals and errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    Session,
    ApiToken,
}

impl AuthType {
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
}

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
    #[error("internal error")]
    Internal,
}

impl From<crate::users::StoreError> for AuthError {
    fn from(_: crate::users::StoreError) -> Self {
        AuthError::Internal
    }
}

#[derive(Debug, Clone)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub session_id: String,
    pub access_expires_ms: i64,
    pub refresh_expires_ms: i64,
}

#[derive(Debug, Clone)]
pub enum LoginOutcome {
    Tokens {
        pair: TokenPair,
        user: UserView,
    },
    TotpRequired {
        challenge_id: String,
    },
}

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
/// (via the wizard's SystemOps), memory in tests. Secrets never
/// touch `admin-users.json`.
#[async_trait::async_trait]
pub trait CredentialStore: Send + Sync {
    async fn read(&self, target: &str) -> Option<String>;
    async fn write(&self, target: &str, secret: &str) -> Result<(), String>;
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

const LOCK_THRESHOLD: usize = 5;
const LOCK_WINDOW_MS: i64 = 15 * 60 * 1000;
const LOCK_BASE_MS: i64 = 15 * 60 * 1000;
const LOCK_MAX_MS: i64 = 60 * 60 * 1000;
const IP_FAILURE_LIMIT: usize = 30;
const CHALLENGE_TTL_MS: i64 = 5 * 60 * 1000;
const ENROLLMENT_TTL_MS: i64 = 10 * 60 * 1000;

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

pub struct AuthService {
    store: Arc<UserStore>,
    creds: Arc<dyn CredentialStore>,
    lockouts: Mutex<HashMap<String, LockState>>,
    ip_failures: Mutex<HashMap<String, Vec<i64>>>,
    challenges: Mutex<HashMap<String, TotpChallenge>>,
    enrollments: Mutex<HashMap<String, Enrollment>>,
    pending_totp: Mutex<HashMap<String, PendingTotp>>,
}

impl AuthService {
    pub fn new(store: Arc<UserStore>, creds: Arc<dyn CredentialStore>) -> Arc<Self> {
        Arc::new(Self {
            store,
            creds,
            lockouts: Mutex::new(HashMap::new()),
            ip_failures: Mutex::new(HashMap::new()),
            challenges: Mutex::new(HashMap::new()),
            enrollments: Mutex::new(HashMap::new()),
            pending_totp: Mutex::new(HashMap::new()),
        })
    }

    pub fn store(&self) -> &Arc<UserStore> {
        &self.store
    }

    // -- lockout bookkeeping -------------------------------------------

    async fn is_locked(&self, username_key: &str) -> bool {
        let guard = self.lockouts.lock().await;
        guard
            .get(username_key)
            .map(|s| s.locked_until_ms > now_ms())
            .unwrap_or(false)
    }

    async fn record_failure(&self, username_key: &str, source_ip: &str) {
        let now = now_ms();
        {
            let mut guard = self.lockouts.lock().await;
            let st = guard.entry(username_key.to_string()).or_default();
            st.failures.retain(|t| now - t < LOCK_WINDOW_MS);
            st.failures.push(now);
            if st.failures.len() >= LOCK_THRESHOLD {
                let backoff = (LOCK_BASE_MS << st.lock_count.min(2)).min(LOCK_MAX_MS);
                st.locked_until_ms = now + backoff;
                st.lock_count += 1;
                st.failures.clear();
            }
        }
        let mut ips = self.ip_failures.lock().await;
        let v = ips.entry(source_ip.to_string()).or_default();
        v.retain(|t| now - t < LOCK_WINDOW_MS);
        v.push(now);
    }

    async fn ip_throttled(&self, source_ip: &str) -> bool {
        let now = now_ms();
        let guard = self.ip_failures.lock().await;
        guard
            .get(source_ip)
            .map(|v| v.iter().filter(|t| now - *t < LOCK_WINDOW_MS).count() >= IP_FAILURE_LIMIT)
            .unwrap_or(false)
    }

    async fn record_success(&self, username_key: &str) {
        let mut guard = self.lockouts.lock().await;
        guard.remove(username_key);
    }

    // -- login ----------------------------------------------------------

    pub async fn login(
        &self,
        username: &str,
        password: &str,
        device_name: &str,
        source_ip: &str,
    ) -> Result<LoginOutcome, AuthError> {
        let key = username.to_lowercase();
        let locked = self.is_locked(&key).await;
        let throttled = self.ip_throttled(source_ip).await;
        let user = self
            .store
            .read(|f| f.users.iter().find(|u| u.username.eq_ignore_ascii_case(username)).cloned())
            .await;

        // Pay the argon2 cost in every path (dummy hash for unknown
        // users) so timing does not enumerate accounts.
        let verified = match &user {
            Some(u) => verify_password(password, &u.password_hash),
            None => {
                let _ = verify_password(password, crate::users::dummy_hash());
                false
            }
        };

        let user = match (user, verified) {
            (Some(u), true) if !locked && !throttled && !u.disabled => u,
            _ => {
                self.record_failure(&key, source_ip).await;
                return Err(AuthError::InvalidCredentials);
            }
        };

        self.record_success(&key).await;
        if user.totp_enabled {
            let challenge_id = generate_token();
            self.challenges.lock().await.insert(
                challenge_id.clone(),
                TotpChallenge {
                    user_id: user.id.clone(),
                    device_name: device_name.to_string(),
                    expires_ms: now_ms() + CHALLENGE_TTL_MS,
                },
            );
            return Ok(LoginOutcome::TotpRequired { challenge_id });
        }
        let pair = self.create_session(&user.id, device_name).await?;
        Ok(LoginOutcome::Tokens {
            pair,
            user: UserView::from(&user),
        })
    }

    /// Second login step when TOTP is enabled. Accepts a current
    /// TOTP code or an unused recovery code.
    pub async fn complete_totp_login(
        &self,
        challenge_id: &str,
        code: &str,
        source_ip: &str,
    ) -> Result<(TokenPair, UserView), AuthError> {
        let challenge = self.challenges.lock().await.remove(challenge_id);
        let challenge = match challenge {
            Some(c) if c.expires_ms > now_ms() => c,
            _ => return Err(AuthError::InvalidCredentials),
        };
        let user = self
            .store
            .read(|f| f.users.iter().find(|u| u.id == challenge.user_id).cloned())
            .await
            .ok_or(AuthError::InvalidCredentials)?;
        if user.disabled {
            return Err(AuthError::InvalidCredentials);
        }
        if self.verify_user_totp(&user, code).await {
            // Re-insert nothing: the challenge is consumed on success
            // and on failure (one guess per challenge; the client
            // logs in again for another).
            let pair = self.create_session(&user.id, &challenge.device_name).await?;
            return Ok((pair, UserView::from(&user)));
        }
        self.record_failure(&user.username.to_lowercase(), source_ip)
            .await;
        Err(AuthError::InvalidCredentials)
    }

    /// Verify a TOTP or recovery code for a user. Recovery codes are
    /// single-use and consumed here.
    pub async fn verify_user_totp(&self, user: &User, code: &str) -> bool {
        let code = code.trim();
        // Recovery codes first (they are longer than 6 digits).
        if code.len() > 6 {
            let hash = hash_token(&code.to_uppercase());
            if user.recovery_code_hashes.iter().any(|h| h == &hash) {
                let user_id = user.id.clone();
                let _ = self
                    .store
                    .mutate::<_, AuthError>(|f| {
                        if let Some(u) = f.users.iter_mut().find(|u| u.id == user_id) {
                            u.recovery_code_hashes.retain(|h| h != &hash);
                        }
                        Ok(())
                    })
                    .await;
                return true;
            }
            return false;
        }
        let Some(secret_b32) = self.creds.read(&totp_target(&user.id)).await else {
            return false;
        };
        let Some(secret) = base32_decode(&secret_b32) else {
            return false;
        };
        totp_verify(&secret, code, now_ms() / 1000)
    }

    // -- sessions -------------------------------------------------------

    pub async fn create_session(
        &self,
        user_id: &str,
        device_name: &str,
    ) -> Result<TokenPair, AuthError> {
        let access = generate_token();
        let refresh = generate_token();
        let now = now_ms();
        let session = Session {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            device_name: device_name.to_string(),
            access_hash: hash_token(&access),
            refresh_hash: hash_token(&refresh),
            retired_refresh_hashes: Vec::new(),
            created_ms: now,
            access_expires_ms: now + ACCESS_TTL_MS,
            refresh_expires_ms: now + REFRESH_TTL_MS,
            last_used_ms: now,
            revoked: false,
        };
        let pair = TokenPair {
            access_token: access,
            refresh_token: refresh,
            session_id: session.id.clone(),
            access_expires_ms: session.access_expires_ms,
            refresh_expires_ms: session.refresh_expires_ms,
        };
        self.store
            .mutate::<_, AuthError>(|f| {
                f.sessions.push(session);
                Ok(())
            })
            .await?;
        Ok(pair)
    }

    /// Rotate a refresh token. Presenting a retired token revokes
    /// the whole session family (theft detection).
    pub async fn refresh(&self, refresh_token: &str) -> Result<(TokenPair, UserView), AuthError> {
        let hash = hash_token(refresh_token);
        let now = now_ms();
        let found = self
            .store
            .read(|f| {
                f.sessions.iter().find_map(|s| {
                    if s.refresh_hash == hash {
                        Some((s.id.clone(), false))
                    } else if s.retired_refresh_hashes.iter().any(|h| h == &hash) {
                        Some((s.id.clone(), true))
                    } else {
                        None
                    }
                })
            })
            .await;
        let Some((session_id, retired)) = found else {
            return Err(AuthError::InvalidCredentials);
        };
        if retired {
            let sid = session_id.clone();
            let _ = self
                .store
                .mutate::<_, AuthError>(|f| {
                    if let Some(s) = f.sessions.iter_mut().find(|s| s.id == sid) {
                        s.revoked = true;
                    }
                    Ok(())
                })
                .await;
            return Err(AuthError::SessionRevoked);
        }
        let access = generate_token();
        let refresh = generate_token();
        let pair = self
            .store
            .mutate::<Option<TokenPair>, AuthError>(|f| {
                let Some(s) = f.sessions.iter_mut().find(|s| s.id == session_id) else {
                    return Ok(None);
                };
                if s.revoked || s.refresh_expires_ms <= now {
                    return Ok(None);
                }
                s.retired_refresh_hashes.push(s.refresh_hash.clone());
                s.access_hash = hash_token(&access);
                s.refresh_hash = hash_token(&refresh);
                s.access_expires_ms = now + ACCESS_TTL_MS;
                s.refresh_expires_ms = now + REFRESH_TTL_MS;
                s.last_used_ms = now;
                Ok(Some(TokenPair {
                    access_token: access.clone(),
                    refresh_token: refresh.clone(),
                    session_id: s.id.clone(),
                    access_expires_ms: s.access_expires_ms,
                    refresh_expires_ms: s.refresh_expires_ms,
                }))
            })
            .await?
            .ok_or(AuthError::InvalidCredentials)?;
        let user = self
            .store
            .read(|f| {
                let sid = pair.session_id.clone();
                let s = f.sessions.iter().find(|s| s.id == sid)?;
                f.users.iter().find(|u| u.id == s.user_id).cloned()
            })
            .await
            .ok_or(AuthError::InvalidCredentials)?;
        if user.disabled {
            return Err(AuthError::InvalidCredentials);
        }
        Ok((pair, UserView::from(&user)))
    }

    /// Validate a bearer credential (session access token or API
    /// token) into a principal. Records last use.
    pub async fn validate_bearer(&self, token: &str) -> Option<Principal> {
        let hash = hash_token(token);
        let now = now_ms();
        // Session access token?
        let session_user = self
            .store
            .read(|f| {
                let s = f.sessions.iter().find(|s| {
                    s.access_hash == hash && !s.revoked && s.access_expires_ms > now
                })?;
                let u = f.users.iter().find(|u| u.id == s.user_id && !u.disabled)?;
                Some((s.id.clone(), u.clone()))
            })
            .await;
        if let Some((session_id, user)) = session_user {
            self.touch_session(&session_id).await;
            return Some(Principal {
                user_id: user.id.clone(),
                username: user.username.clone(),
                role: user.role.to_role(),
                auth_type: AuthType::Session,
                session_id,
            });
        }
        // API token?
        let api = self
            .store
            .read(|f| {
                let t = f.api_tokens.iter().find(|t| {
                    t.hash == hash
                        && !t.revoked
                        && t.expires_ms.map(|e| e > now).unwrap_or(true)
                })?;
                let u = f.users.iter().find(|u| u.id == t.user_id && !u.disabled)?;
                Some((t.id.clone(), t.role, u.clone()))
            })
            .await;
        if let Some((token_id, role, user)) = api {
            self.touch_api_token(&token_id).await;
            return Some(Principal {
                user_id: user.id.clone(),
                username: user.username.clone(),
                role: role.to_role(),
                auth_type: AuthType::ApiToken,
                session_id: token_id,
            });
        }
        None
    }

    async fn touch_session(&self, session_id: &str) {
        let now = now_ms();
        let stale = self
            .store
            .read(|f| {
                f.sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .map(|s| now - s.last_used_ms > 60_000)
                    .unwrap_or(false)
            })
            .await;
        if stale {
            let sid = session_id.to_string();
            let _ = self
                .store
                .mutate::<_, AuthError>(|f| {
                    if let Some(s) = f.sessions.iter_mut().find(|s| s.id == sid) {
                        s.last_used_ms = now;
                    }
                    Ok(())
                })
                .await;
        }
    }

    async fn touch_api_token(&self, token_id: &str) {
        let now = now_ms();
        let tid = token_id.to_string();
        let _ = self
            .store
            .mutate::<_, AuthError>(|f| {
                if let Some(t) = f.api_tokens.iter_mut().find(|t| t.id == tid) {
                    t.last_used_ms = Some(now);
                }
                Ok(())
            })
            .await;
    }

    pub async fn revoke_session(&self, session_id: &str) -> Result<(), AuthError> {
        let sid = session_id.to_string();
        self.store
            .mutate::<_, AuthError>(|f| {
                if let Some(s) = f.sessions.iter_mut().find(|s| s.id == sid) {
                    s.revoked = true;
                }
                Ok(())
            })
            .await
    }

    pub async fn revoke_user_sessions(&self, user_id: &str, except: Option<&str>) {
        let uid = user_id.to_string();
        let except = except.map(str::to_string);
        let _ = self
            .store
            .mutate::<_, AuthError>(|f| {
                for s in f.sessions.iter_mut() {
                    if s.user_id == uid && except.as_deref() != Some(&s.id) {
                        s.revoked = true;
                    }
                }
                Ok(())
            })
            .await;
    }

    // -- passwords ------------------------------------------------------

    pub async fn change_password(
        &self,
        user_id: &str,
        current: &str,
        new_password: &str,
        keep_session: Option<&str>,
    ) -> Result<(), AuthError> {
        let user = self
            .store
            .read(|f| f.users.iter().find(|u| u.id == user_id).cloned())
            .await
            .ok_or(AuthError::NotFound("user".into()))?;
        if !verify_password(current, &user.password_hash) {
            return Err(AuthError::InvalidCredentials);
        }
        if let Some(e) = password_policy_error(new_password, &user.username) {
            return Err(AuthError::InvalidInput(e));
        }
        let hash = hash_password(new_password).map_err(|_| AuthError::Internal)?;
        let uid = user_id.to_string();
        self.store
            .mutate::<_, AuthError>(|f| {
                if let Some(u) = f.users.iter_mut().find(|u| u.id == uid) {
                    u.password_hash = hash;
                    u.must_change_password = false;
                    u.updated_ms = now_ms();
                }
                Ok(())
            })
            .await?;
        self.revoke_user_sessions(user_id, keep_session).await;
        Ok(())
    }

    /// Admin reset: a generated temporary password, must-change set,
    /// all sessions revoked. Returns the temporary password (shown
    /// once to the admin).
    pub async fn admin_reset_password(&self, user_id: &str) -> Result<String, AuthError> {
        let temp = generate_token()[..16].to_string();
        let hash = hash_password(&temp).map_err(|_| AuthError::Internal)?;
        let uid = user_id.to_string();
        let found = self
            .store
            .mutate::<bool, AuthError>(|f| {
                if let Some(u) = f.users.iter_mut().find(|u| u.id == uid) {
                    u.password_hash = hash;
                    u.must_change_password = true;
                    u.updated_ms = now_ms();
                    Ok(true)
                } else {
                    Ok(false)
                }
            })
            .await?;
        if !found {
            return Err(AuthError::NotFound("user".into()));
        }
        self.revoke_user_sessions(user_id, None).await;
        Ok(temp)
    }

    // -- user administration ---------------------------------------------

    pub async fn create_user(
        &self,
        username: &str,
        password: &str,
        role: RoleName,
    ) -> Result<UserView, AuthError> {
        let username = username.trim().to_string();
        if username.is_empty() || username.len() > 64 {
            return Err(AuthError::InvalidInput("username is empty or too long".into()));
        }
        if let Some(e) = password_policy_error(password, &username) {
            return Err(AuthError::InvalidInput(e));
        }
        let hash = hash_password(password).map_err(|_| AuthError::Internal)?;
        let now = now_ms();
        let user = User {
            id: uuid::Uuid::new_v4().to_string(),
            username: username.clone(),
            password_hash: hash,
            role,
            disabled: false,
            must_change_password: false,
            totp_enabled: false,
            recovery_code_hashes: Vec::new(),
            created_ms: now,
            updated_ms: now,
        };
        let view = UserView::from(&user);
        self.store
            .mutate::<_, AuthError>(|f| {
                if f.users
                    .iter()
                    .any(|u| u.username.eq_ignore_ascii_case(&username))
                {
                    return Err(AuthError::InvalidInput(format!(
                        "username `{username}` already exists"
                    )));
                }
                f.users.push(user);
                Ok(())
            })
            .await?;
        Ok(view)
    }

    /// Guards from the auth plan: the last enabled admin cannot be
    /// deleted, disabled, or demoted; a user cannot delete themself.
    fn guard_admin_removal(f: &crate::users::UsersFile, target: &User) -> Result<(), AuthError> {
        if target.role == RoleName::Admin && !target.disabled {
            let enabled_admins = f
                .users
                .iter()
                .filter(|u| u.role == RoleName::Admin && !u.disabled)
                .count();
            if enabled_admins <= 1 {
                return Err(AuthError::Forbidden(
                    "the last enabled admin cannot be removed or demoted".into(),
                ));
            }
        }
        Ok(())
    }

    pub async fn update_user(
        &self,
        user_id: &str,
        role: Option<RoleName>,
        disabled: Option<bool>,
    ) -> Result<UserView, AuthError> {
        let uid = user_id.to_string();
        let view = self
            .store
            .mutate::<Option<UserView>, AuthError>(|f| {
                let Some(u) = f.users.iter().find(|u| u.id == uid).cloned() else {
                    return Ok(None);
                };
                let demoting = role.map(|r| r != RoleName::Admin).unwrap_or(false);
                let disabling = disabled == Some(true);
                if (demoting || disabling) && u.role == RoleName::Admin {
                    Self::guard_admin_removal(f, &u)?;
                }
                let u = f.users.iter_mut().find(|u| u.id == uid).unwrap();
                if let Some(r) = role {
                    u.role = r;
                }
                if let Some(d) = disabled {
                    u.disabled = d;
                }
                u.updated_ms = now_ms();
                Ok(Some(UserView::from(&*u)))
            })
            .await?
            .ok_or(AuthError::NotFound("user".into()))?;
        // Role change or disable revokes sessions immediately.
        if role.is_some() || disabled == Some(true) {
            self.revoke_user_sessions(user_id, None).await;
        }
        Ok(view)
    }

    pub async fn delete_user(&self, actor_id: &str, user_id: &str) -> Result<(), AuthError> {
        if actor_id == user_id {
            return Err(AuthError::Forbidden("a user cannot delete themself".into()));
        }
        let uid = user_id.to_string();
        let removed = self
            .store
            .mutate::<bool, AuthError>(|f| {
                let Some(u) = f.users.iter().find(|u| u.id == uid).cloned() else {
                    return Ok(false);
                };
                Self::guard_admin_removal(f, &u)?;
                f.users.retain(|u| u.id != uid);
                f.sessions.retain(|s| s.user_id != uid);
                f.api_tokens.retain(|t| t.user_id != uid);
                Ok(true)
            })
            .await?;
        if !removed {
            return Err(AuthError::NotFound("user".into()));
        }
        self.creds.delete(&totp_target(user_id)).await;
        Ok(())
    }

    // -- API tokens --------------------------------------------------------

    pub async fn create_api_token(
        &self,
        issuer: &Principal,
        user_id: &str,
        name: &str,
        role: RoleName,
        expires_days: Option<i64>,
    ) -> Result<(ApiToken, String), AuthError> {
        if role == RoleName::Admin || role == RoleName::Configurator {
            return Err(AuthError::Forbidden(
                "API tokens are issued at viewer or operator role".into(),
            ));
        }
        let exists = self
            .store
            .read(|f| f.users.iter().any(|u| u.id == user_id && !u.disabled))
            .await;
        if !exists {
            return Err(AuthError::NotFound("user".into()));
        }
        let value = generate_token();
        let now = now_ms();
        let token = ApiToken {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            name: name.to_string(),
            hash: hash_token(&value),
            role,
            issued_by: issuer.user_id.clone(),
            created_ms: now,
            expires_ms: expires_days.map(|d| now + d * 24 * 60 * 60 * 1000),
            last_used_ms: None,
            revoked: false,
        };
        let cloned = token.clone();
        self.store
            .mutate::<_, AuthError>(|f| {
                f.api_tokens.push(token);
                Ok(())
            })
            .await?;
        Ok((cloned, value))
    }

    pub async fn revoke_api_token(&self, token_id: &str) -> Result<(), AuthError> {
        let tid = token_id.to_string();
        let found = self
            .store
            .mutate::<bool, AuthError>(|f| {
                if let Some(t) = f.api_tokens.iter_mut().find(|t| t.id == tid) {
                    t.revoked = true;
                    Ok(true)
                } else {
                    Ok(false)
                }
            })
            .await?;
        if !found {
            return Err(AuthError::NotFound("api token".into()));
        }
        Ok(())
    }

    // -- TOTP enrollment ----------------------------------------------------

    /// Begin TOTP enrollment: a fresh secret is generated and
    /// returned with its otpauth URI (shown once as a QR at the
    /// desktop / authenticator entry). The secret is held only as
    /// an expiring in-memory pending entry (code-review B-6): it is
    /// written to the credential store — becoming the live factor —
    /// only when `totp_verify_enrollment` confirms it, so starting
    /// a re-enrollment can never overwrite or invalidate a live
    /// factor before the replacement is proven.
    ///
    /// Replacing a live factor additionally requires step-up:
    /// when the user already has TOTP enabled, `step_up_password`
    /// must be the account password (the same step-up
    /// `totp_disable` requires). First enrollment needs none.
    pub async fn totp_begin_enrollment(
        &self,
        user_id: &str,
        username: &str,
        step_up_password: Option<&str>,
    ) -> Result<(String, String), AuthError> {
        let user = self
            .store
            .read(|f| f.users.iter().find(|u| u.id == user_id).cloned())
            .await
            .ok_or_else(|| AuthError::NotFound("user".into()))?;
        if user.totp_enabled {
            let ok = step_up_password
                .is_some_and(|p| verify_password(p, &user.password_hash));
            if !ok {
                return Err(AuthError::InvalidCredentials);
            }
        }
        let mut secret_bytes = [0u8; 20];
        for i in 0..5 {
            let b = uuid::Uuid::new_v4().into_bytes();
            secret_bytes[i * 4..i * 4 + 4].copy_from_slice(&b[..4]);
        }
        let secret = base32_encode(&secret_bytes);
        self.pending_totp.lock().await.insert(
            user_id.to_string(),
            PendingTotp {
                secret: secret.clone(),
                expires_ms: now_ms() + PENDING_TOTP_TTL_MS,
            },
        );
        let uri = format!(
            "otpauth://totp/LLMdil:{username}?secret={secret}&issuer=LLMdil&digits=6&period=30"
        );
        Ok((secret, uri))
    }

    /// Confirm enrollment with a code from the authenticator. On
    /// success the pending secret is promoted into the credential
    /// store as the live factor, TOTP is enabled, and 8 single-use
    /// recovery codes are returned (shown once, stored hashed).
    /// An expired pending enrollment is refused and dropped.
    pub async fn totp_verify_enrollment(
        &self,
        user_id: &str,
        code: &str,
    ) -> Result<Vec<String>, AuthError> {
        let pending = self.pending_totp.lock().await.remove(user_id);
        let Some(pending) = pending else {
            return Err(AuthError::InvalidInput(
                "no TOTP enrollment in progress".into(),
            ));
        };
        if pending.expires_ms <= now_ms() {
            return Err(AuthError::InvalidInput(
                "TOTP enrollment expired; begin enrollment again".into(),
            ));
        }
        let secret_b32 = pending.secret.clone();
        let secret = base32_decode(&secret_b32).ok_or(AuthError::Internal)?;
        if !totp_verify(&secret, code.trim(), now_ms() / 1000) {
            // Restore the pending enrollment so the user can retry
            // with the same secret until it expires.
            self.pending_totp
                .lock()
                .await
                .insert(user_id.to_string(), pending);
            return Err(AuthError::InvalidCredentials);
        }
        // Promotion point: only now does the new secret become the
        // live credential (code-review B-6).
        self.creds
            .write(&totp_target(user_id), &secret_b32)
            .await
            .map_err(|_| AuthError::Internal)?;
        let codes: Vec<String> = (0..8).map(|_| recovery_code()).collect();
        let hashes: Vec<String> = codes
            .iter()
            .map(|c| hash_token(&c.to_uppercase()))
            .collect();
        let uid = user_id.to_string();
        self.store
            .mutate::<_, AuthError>(|f| {
                if let Some(u) = f.users.iter_mut().find(|u| u.id == uid) {
                    u.totp_enabled = true;
                    u.recovery_code_hashes = hashes;
                    u.updated_ms = now_ms();
                }
                Ok(())
            })
            .await?;
        Ok(codes)
    }

    pub async fn totp_disable(&self, user_id: &str) -> Result<(), AuthError> {
        let uid = user_id.to_string();
        self.store
            .mutate::<_, AuthError>(|f| {
                if let Some(u) = f.users.iter_mut().find(|u| u.id == uid) {
                    u.totp_enabled = false;
                    u.recovery_code_hashes.clear();
                    u.updated_ms = now_ms();
                }
                Ok(())
            })
            .await?;
        self.pending_totp.lock().await.remove(user_id);
        self.creds.delete(&totp_target(user_id)).await;
        Ok(())
    }

    // -- enrollment codes ---------------------------------------------------

    /// Create a one-time enrollment code for a user (desktop-side
    /// pairing). Redeeming it creates a per-device session for that
    /// user without a password — the desktop operator's physical
    /// presence and choice of user/role is the authorizing act.
    pub async fn create_enrollment_code(&self, user_id: &str) -> Result<String, AuthError> {
        let exists = self
            .store
            .read(|f| f.users.iter().any(|u| u.id == user_id && !u.disabled))
            .await;
        if !exists {
            return Err(AuthError::NotFound("user".into()));
        }
        let code = recovery_code();
        self.enrollments.lock().await.insert(
            hash_token(&code.to_uppercase()),
            Enrollment {
                user_id: user_id.to_string(),
                expires_ms: now_ms() + ENROLLMENT_TTL_MS,
            },
        );
        Ok(code)
    }

    pub async fn redeem_enrollment_code(
        &self,
        code: &str,
        device_name: &str,
    ) -> Result<(TokenPair, UserView), AuthError> {
        let key = hash_token(&code.trim().to_uppercase());
        let enrollment = self.enrollments.lock().await.remove(&key);
        let enrollment = match enrollment {
            Some(e) if e.expires_ms > now_ms() => e,
            _ => return Err(AuthError::InvalidCredentials),
        };
        let user = self
            .store
            .read(|f| {
                f.users
                    .iter()
                    .find(|u| u.id == enrollment.user_id && !u.disabled)
                    .cloned()
            })
            .await
            .ok_or(AuthError::InvalidCredentials)?;
        let pair = self.create_session(&user.id, device_name).await?;
        Ok((pair, UserView::from(&user)))
    }
}

// ---------------------------------------------------------------------------
// TOTP (RFC 6238, HMAC-SHA1, 30 s step, 6 digits, ±1 step)
// ---------------------------------------------------------------------------

pub fn totp_at(secret: &[u8], unix_secs: i64) -> u32 {
    let counter = (unix_secs / 30) as u64;
    let msg = counter.to_be_bytes();
    use hmac::Mac;
    let mut mac = <hmac::Hmac<sha1::Sha1> as hmac::Mac>::new_from_slice(secret)
        .expect("HMAC accepts any key length");
    mac.update(&msg);
    let out = mac.finalize().into_bytes();
    let offset = (out[19] & 0x0f) as usize;
    let binary = ((u32::from(out[offset]) & 0x7f) << 24)
        | ((u32::from(out[offset + 1])) << 16)
        | ((u32::from(out[offset + 2])) << 8)
        | u32::from(out[offset + 3]);
    binary % 1_000_000
}

pub fn totp_verify(secret: &[u8], code: &str, unix_secs: i64) -> bool {
    let Ok(code_num) = code.parse::<u32>() else {
        return false;
    };
    [-30, 0, 30].iter().any(|step| {
        // Constant-shape comparison; the values are short-lived codes.
        totp_at(secret, unix_secs + step) == code_num
    })
}

fn recovery_code() -> String {
    // XXXX-XXXX from a Crockford-style alphabet.
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let pick = |i: usize| ALPHABET[(bytes[i] % 32) as usize] as char;
    format!(
        "{}{}{}{}-{}{}{}{}",
        pick(0),
        pick(1),
        pick(2),
        pick(3),
        pick(4),
        pick(5),
        pick(6),
        pick(7)
    )
}

const B32: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        buffer = (buffer << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.chars() {
        if c == '=' || c.is_whitespace() {
            continue;
        }
        let val = B32.iter().position(|&x| x == c as u8)? as u32;
        buffer = (buffer << 5) | val;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::UserStore;

    async fn service() -> Arc<AuthService> {
        AuthService::new(
            UserStore::in_memory(),
            Arc::new(MemoryCredentialStore::default()),
        )
    }

    #[test]
    fn totp_rfc_vectors() {
        // RFC 6238 test secret "12345678901234567890" (ASCII), SHA-1.
        let secret = b"12345678901234567890";
        assert_eq!(totp_at(secret, 59), 287082);
        assert_eq!(totp_at(secret, 1111111109), 81804);
        assert!(totp_verify(secret, "287082", 59));
        assert!(totp_verify(secret, "287082", 89)); // +1 step window
        assert!(!totp_verify(secret, "111111", 59));
        let b32 = base32_encode(secret);
        assert_eq!(base32_decode(&b32).unwrap(), secret.to_vec());
    }

    #[tokio::test]
    async fn login_session_refresh_rotation_and_reuse() {
        let svc = service().await;
        svc.create_user("alice", "a-unique-passphrase", RoleName::Operator)
            .await
            .unwrap();
        let outcome = svc
            .login("alice", "a-unique-passphrase", "phone", "127.0.0.1")
            .await
            .unwrap();
        let LoginOutcome::Tokens { pair, user } = outcome else {
            panic!("expected tokens");
        };
        assert_eq!(user.username, "alice");
        let principal = svc.validate_bearer(&pair.access_token).await.unwrap();
        assert_eq!(principal.role, Role::Operator);

        // Rotate: old refresh retires; new pair works.
        let (pair2, _) = svc.refresh(&pair.refresh_token).await.unwrap();
        assert_ne!(pair2.access_token, pair.access_token);
        // The old access token no longer validates.
        assert!(svc.validate_bearer(&pair.access_token).await.is_none());
        // Reuse of the retired refresh token revokes the family.
        let err = svc.refresh(&pair.refresh_token).await.unwrap_err();
        assert_eq!(err, AuthError::SessionRevoked);
        assert!(svc.validate_bearer(&pair2.access_token).await.is_none());
    }

    #[tokio::test]
    async fn lockout_after_five_failures() {
        let svc = service().await;
        svc.create_user("bob", "a-unique-passphrase", RoleName::Viewer)
            .await
            .unwrap();
        for _ in 0..5 {
            let err = svc
                .login("bob", "wrong-password-zz", "phone", "127.0.0.1")
                .await
                .unwrap_err();
            assert_eq!(err, AuthError::InvalidCredentials);
        }
        // Correct password during the lock: same indistinguishable error.
        let err = svc
            .login("bob", "a-unique-passphrase", "phone", "127.0.0.1")
            .await
            .unwrap_err();
        assert_eq!(err, AuthError::InvalidCredentials);
    }

    #[tokio::test]
    async fn admin_guards() {
        let svc = service().await;
        let admin = svc
            .create_user("root", "a-unique-passphrase", RoleName::Admin)
            .await
            .unwrap();
        // Last enabled admin cannot be disabled or demoted.
        assert!(svc
            .update_user(&admin.id, None, Some(true))
            .await
            .is_err());
        assert!(svc
            .update_user(&admin.id, Some(RoleName::Viewer), None)
            .await
            .is_err());
        // No self-delete.
        assert!(svc.delete_user(&admin.id, &admin.id).await.is_err());
    }

    #[tokio::test]
    async fn totp_enrollment_and_login() {
        let svc = service().await;
        let user = svc
            .create_user("carol", "a-unique-passphrase", RoleName::Viewer)
            .await
            .unwrap();
        let (secret_b32, uri) = svc
            .totp_begin_enrollment(&user.id, "carol", None)
            .await
            .unwrap();
        assert!(uri.starts_with("otpauth://totp/LLMdil:carol"));
        let secret = base32_decode(&secret_b32).unwrap();
        let code = format!("{:06}", totp_at(&secret, now_ms() / 1000));
        let codes = svc.totp_verify_enrollment(&user.id, &code).await.unwrap();
        assert_eq!(codes.len(), 8);
        // Password login now requires the second factor.
        let outcome = svc
            .login("carol", "a-unique-passphrase", "phone", "127.0.0.1")
            .await
            .unwrap();
        let LoginOutcome::TotpRequired { challenge_id } = outcome else {
            panic!("expected totp challenge");
        };
        let code2 = format!("{:06}", totp_at(&secret, now_ms() / 1000));
        let (pair, _) = svc
            .complete_totp_login(&challenge_id, &code2, "127.0.0.1")
            .await
            .unwrap();
        assert!(svc.validate_bearer(&pair.access_token).await.is_some());
        // A recovery code works exactly once (fresh challenge each time).
        let outcome = svc
            .login("carol", "a-unique-passphrase", "phone", "127.0.0.1")
            .await
            .unwrap();
        let LoginOutcome::TotpRequired { challenge_id } = outcome else {
            panic!("expected totp challenge");
        };
        svc.complete_totp_login(&challenge_id, &codes[0], "127.0.0.1")
            .await
            .unwrap();
        let outcome = svc
            .login("carol", "a-unique-passphrase", "phone", "127.0.0.1")
            .await
            .unwrap();
        let LoginOutcome::TotpRequired { challenge_id } = outcome else {
            panic!("expected totp challenge");
        };
        assert!(svc
            .complete_totp_login(&challenge_id, &codes[0], "127.0.0.1")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn totp_reenrollment_requires_step_up_and_keeps_live_factor_until_verified() {
        // Code-review B-6: beginning a re-enrollment must neither
        // proceed without a password step-up nor touch the live
        // factor; the pending secret replaces it only on verify.
        let svc = service().await;
        let user = svc
            .create_user("erin", "a-unique-passphrase", RoleName::Viewer)
            .await
            .unwrap();
        let (old_b32, _) = svc
            .totp_begin_enrollment(&user.id, "erin", None)
            .await
            .unwrap();
        let old_secret = base32_decode(&old_b32).unwrap();
        let code = format!("{:06}", totp_at(&old_secret, now_ms() / 1000));
        svc.totp_verify_enrollment(&user.id, &code).await.unwrap();
        assert_eq!(
            svc.creds.read(&totp_target(&user.id)).await.as_deref(),
            Some(old_b32.as_str())
        );
        // No step-up, and a wrong password, are both refused.
        assert!(svc
            .totp_begin_enrollment(&user.id, "erin", None)
            .await
            .is_err());
        assert!(svc
            .totp_begin_enrollment(&user.id, "erin", Some("wrong-passphrase"))
            .await
            .is_err());
        // With the password, a pending secret is issued — and the
        // live credential is still the old one.
        let (new_b32, _) = svc
            .totp_begin_enrollment(&user.id, "erin", Some("a-unique-passphrase"))
            .await
            .unwrap();
        assert_ne!(new_b32, old_b32);
        assert_eq!(
            svc.creds.read(&totp_target(&user.id)).await.as_deref(),
            Some(old_b32.as_str()),
            "begin must not overwrite the live factor"
        );
        let stored = svc
            .store
            .read(|f| f.users.iter().find(|u| u.id == user.id).cloned())
            .await
            .unwrap();
        let old_code = format!("{:06}", totp_at(&old_secret, now_ms() / 1000));
        assert!(svc.verify_user_totp(&stored, &old_code).await);
        // Verifying the pending enrollment promotes the new secret.
        let new_secret = base32_decode(&new_b32).unwrap();
        let new_code = format!("{:06}", totp_at(&new_secret, now_ms() / 1000));
        svc.totp_verify_enrollment(&user.id, &new_code)
            .await
            .unwrap();
        assert_eq!(
            svc.creds.read(&totp_target(&user.id)).await.as_deref(),
            Some(new_b32.as_str())
        );
    }

    #[tokio::test]
    async fn totp_pending_enrollment_expires() {
        // Code-review B-6: an unconfirmed enrollment expires and
        // never reaches the credential store.
        let svc = service().await;
        let user = svc
            .create_user("frank", "a-unique-passphrase", RoleName::Viewer)
            .await
            .unwrap();
        let (secret_b32, _) = svc
            .totp_begin_enrollment(&user.id, "frank", None)
            .await
            .unwrap();
        assert!(svc.creds.read(&totp_target(&user.id)).await.is_none());
        svc.pending_totp
            .lock()
            .await
            .get_mut(&user.id)
            .unwrap()
            .expires_ms = now_ms() - 1;
        let secret = base32_decode(&secret_b32).unwrap();
        let code = format!("{:06}", totp_at(&secret, now_ms() / 1000));
        let err = svc
            .totp_verify_enrollment(&user.id, &code)
            .await
            .expect_err("expired pending enrollment must be refused");
        assert!(matches!(err, AuthError::InvalidInput(_)), "{err:?}");
        assert!(svc.creds.read(&totp_target(&user.id)).await.is_none());
    }

    #[tokio::test]
    async fn api_token_lifecycle() {
        let svc = service().await;
        let admin = svc
            .create_user("root", "a-unique-passphrase", RoleName::Admin)
            .await
            .unwrap();
        let viewer = svc
            .create_user("phone", "a-unique-passphrase", RoleName::Viewer)
            .await
            .unwrap();
        let issuer = Principal {
            user_id: admin.id.clone(),
            username: "root".into(),
            role: Role::Admin,
            auth_type: AuthType::Session,
            session_id: "s".into(),
        };
        let (meta, value) = svc
            .create_api_token(&issuer, &viewer.id, "pixel", RoleName::Viewer, None)
            .await
            .unwrap();
        let p = svc.validate_bearer(&value).await.unwrap();
        assert_eq!(p.auth_type, AuthType::ApiToken);
        assert_eq!(p.role, Role::Viewer);
        // Admin-role tokens are refused.
        assert!(svc
            .create_api_token(&issuer, &viewer.id, "bad", RoleName::Admin, None)
            .await
            .is_err());
        svc.revoke_api_token(&meta.id).await.unwrap();
        assert!(svc.validate_bearer(&value).await.is_none());
    }

    #[tokio::test]
    async fn enrollment_code_redeems_once() {
        let svc = service().await;
        let user = svc
            .create_user("dave", "a-unique-passphrase", RoleName::Operator)
            .await
            .unwrap();
        let code = svc.create_enrollment_code(&user.id).await.unwrap();
        let (pair, view) = svc
            .redeem_enrollment_code(&code, "pixel")
            .await
            .unwrap();
        assert_eq!(view.username, "dave");
        assert!(svc.validate_bearer(&pair.access_token).await.is_some());
        assert!(svc.redeem_enrollment_code(&code, "pixel").await.is_err());
    }
}

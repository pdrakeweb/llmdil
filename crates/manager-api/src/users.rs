//! The API user store: `admin-users.json` in the app data dir.
//!
//! Deliberately NOT part of AppConfig (auth plan): a config write
//! can then never mint or elevate an API user. The file holds users,
//! sessions, and API tokens — hashes only (argon2id PHC strings for
//! passwords, SHA-256 for tokens and recovery codes; TOTP secrets
//! live in the OS credential store, never here). Writes are atomic
//! (temp file + rename) and happen on every mutation. The
//! diagnostics-bundle builder must never include this file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use tokio::sync::RwLock;

pub use manager_app::registry::Role;

pub const STORE_FILE: &str = "admin-users.json";
pub const STORE_VERSION: u32 = 1;

/// Access-token lifetime: 60 minutes (all roles, decision 8).
pub const ACCESS_TTL_MS: i64 = 60 * 60 * 1000;
/// Refresh-token lifetime: 30 days, rotated on every use.
pub const REFRESH_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Wall-clock milliseconds (system clock; this module stays free
/// of a chrono dependency).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    /// argon2id PHC string.
    pub password_hash: String,
    pub role: RoleName,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub must_change_password: bool,
    #[serde(default)]
    pub totp_enabled: bool,
    /// SHA-256 hashes of the unused recovery codes.
    #[serde(default)]
    pub recovery_code_hashes: Vec<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// Role as stored/serialized. Mirrors `manager_app::registry::Role`
/// (which is the authorization type); this wrapper exists so the
/// store file and the wire share one stable string form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoleName {
    Viewer,
    Operator,
    Configurator,
    Admin,
}

impl RoleName {
    pub fn to_role(self) -> Role {
        match self {
            RoleName::Viewer => Role::Viewer,
            RoleName::Operator => Role::Operator,
            RoleName::Configurator => Role::Configurator,
            RoleName::Admin => Role::Admin,
        }
    }

    pub fn from_role(r: Role) -> Self {
        match r {
            Role::Viewer => RoleName::Viewer,
            Role::Operator => RoleName::Operator,
            Role::Configurator => RoleName::Configurator,
            Role::Admin => RoleName::Admin,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RoleName::Viewer => "viewer",
            RoleName::Operator => "operator",
            RoleName::Configurator => "configurator",
            RoleName::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "viewer" => Some(Self::Viewer),
            "operator" => Some(Self::Operator),
            "configurator" => Some(Self::Configurator),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub user_id: String,
    pub device_name: String,
    /// SHA-256 of the current access token.
    pub access_hash: String,
    /// SHA-256 of the current refresh token.
    pub refresh_hash: String,
    /// SHA-256 hashes of rotated-out refresh tokens. Presenting one
    /// is treated as theft: the whole session family is revoked.
    #[serde(default)]
    pub retired_refresh_hashes: Vec<String>,
    pub created_ms: i64,
    pub access_expires_ms: i64,
    pub refresh_expires_ms: i64,
    pub last_used_ms: i64,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: String,
    pub user_id: String,
    pub name: String,
    /// SHA-256 of the token value (shown once at creation).
    pub hash: String,
    /// Bound role: viewer or operator in standing practice (never
    /// admin-issued above operator without an explicit decision);
    /// the effective role also dies with the owning user.
    pub role: RoleName,
    pub issued_by: String,
    pub created_ms: i64,
    pub expires_ms: Option<i64>,
    pub last_used_ms: Option<i64>,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsersFile {
    pub version: u32,
    #[serde(default)]
    pub users: Vec<User>,
    #[serde(default)]
    pub sessions: Vec<Session>,
    #[serde(default)]
    pub api_tokens: Vec<ApiToken>,
}

impl Default for UsersFile {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            users: Vec::new(),
            sessions: Vec::new(),
            api_tokens: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("store file is corrupt: {0}")]
    Corrupt(String),
}

pub struct UserStore {
    path: PathBuf,
    inner: RwLock<UsersFile>,
}

impl UserStore {
    /// Load the store from the data dir (missing file = empty store).
    pub async fn load(data_dir: &Path) -> Result<Arc<Self>, StoreError> {
        let path = data_dir.join(STORE_FILE);
        let file = if path.exists() {
            let text = tokio::fs::read_to_string(&path).await?;
            serde_json::from_str(&text)
                .map_err(|e| StoreError::Corrupt(format!("{}: {e}", path.display())))?
        } else {
            UsersFile::default()
        };
        Ok(Arc::new(Self {
            path,
            inner: RwLock::new(file),
        }))
    }

    /// An empty in-memory store (tests).
    pub fn in_memory() -> Arc<Self> {
        Arc::new(Self {
            path: PathBuf::from("/nonexistent/admin-users.json"),
            inner: RwLock::new(UsersFile::default()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn persist(&self, file: &UsersFile) -> Result<(), StoreError> {
        if self.path.parent().is_none() || self.path.starts_with("/nonexistent") {
            return Ok(());
        }
        let text = serde_json::to_string_pretty(file)
            .map_err(|e| StoreError::Corrupt(e.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, text).await?;
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }

    /// Run a mutation against the store and persist the result.
    pub async fn mutate<T, E>(
        &self,
        f: impl FnOnce(&mut UsersFile) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<StoreError>,
    {
        let mut guard = self.inner.write().await;
        let out = f(&mut guard)?;
        self.persist(&guard).await.map_err(E::from)?;
        Ok(out)
    }

    /// Read-only access.
    pub async fn read<T>(&self, f: impl FnOnce(&UsersFile) -> T) -> T {
        let guard = self.inner.read().await;
        f(&guard)
    }
}

// ---------------------------------------------------------------------------
// Passwords
// ---------------------------------------------------------------------------

/// argon2id with the plan's parameters (m = 64 MiB, t = 3, p = 1).
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    let params = Params::new(65536, 3, 1, None).expect("valid argon2 params");
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let salt = SaltString::generate(SaltRng);
    Ok(argon.hash_password(password.as_bytes(), &salt)?.to_string())
}

pub fn verify_password(password: &str, phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// A hash of a fixed nonsense password with the plan's parameters,
/// computed once; login verifies against it for unknown usernames so
/// the timing does not enumerate users. The result is discarded.
pub fn dummy_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| {
        hash_password("dummy-password-for-timing-parity").expect("argon2 available")
    })
}

/// Salt RNG backed by UUID v4 randomness (which is OS-sourced);
/// implements the `rand_core` traits `SaltString::generate` needs,
/// via the re-export in `argon2::password_hash` so versions align.
#[derive(Clone, Copy)]
pub struct SaltRng;

impl argon2::password_hash::rand_core::RngCore for SaltRng {
    fn next_u32(&mut self) -> u32 {
        let b = uuid::Uuid::new_v4().into_bytes();
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }
    fn next_u64(&mut self) -> u64 {
        let b = uuid::Uuid::new_v4().into_bytes();
        u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        let mut filled = 0;
        while filled < dest.len() {
            let b = uuid::Uuid::new_v4().into_bytes();
            let n = (dest.len() - filled).min(16);
            dest[filled..filled + n].copy_from_slice(&b[..n]);
            filled += n;
        }
    }
    fn try_fill_bytes(
        &mut self,
        dest: &mut [u8],
    ) -> Result<(), argon2::password_hash::rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl argon2::password_hash::rand_core::CryptoRng for SaltRng {}

/// Common-password blocklist (worst offenders; a bundled subset of
/// the standard public lists — enough to stop the trivially guessed
/// passwords the plan's policy targets).
const COMMON_PASSWORDS: &[&str] = &[
    "password", "password1", "password123", "123456789012", "1234567890",
    "qwertyuiop", "qwerty123456", "letmein12345", "admin1234567", "administrator",
    "welcome12345", "changeme1234", "iloveyou1234", "football1234", "baseball1234",
    "sunshine1234", "princess1234", "dragon123456", "monkey123456", "shadow123456",
    "master123456", "superman1234", "batman123456", "trustno12345", "starwars1234",
    "pokemon12345", "naruto123456", "computer1234", "internet1234", "michael12345",
    "jordan123456", "hunter123456", "soccer123456", "hockey123456", "killer123456",
    "george123456", "charlie12345", "thomas123456", "jessica12345", "pepper123456",
    "ginger123456", "maggie123456", "summer123456", "winter123456", "spring123456",
    "autumn123456", "secret123456", "passw0rd1234", "p@ssw0rd123", "default12345",
];

/// Password policy (auth plan): minimum 12 characters, no
/// composition rules, blocklist, not equal to the username.
pub fn password_policy_error(password: &str, username: &str) -> Option<String> {
    if password.chars().count() < 12 {
        return Some("password must be at least 12 characters".to_string());
    }
    if password.eq_ignore_ascii_case(username) {
        return Some("password must not equal the username".to_string());
    }
    let lower = password.to_lowercase();
    if COMMON_PASSWORDS.contains(&lower.as_str()) {
        return Some("password is on the common-password blocklist".to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// Token helpers
// ---------------------------------------------------------------------------

/// A fresh opaque token: 256 bits, base64url, shown to the caller
/// once; only its SHA-256 is stored.
pub fn generate_token() -> String {
    let a = uuid::Uuid::new_v4().into_bytes();
    let b = uuid::Uuid::new_v4().into_bytes();
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(&a);
    bytes[16..].copy_from_slice(&b);
    base64url(&bytes)
}

pub fn hash_token(token: &str) -> String {
    let digest = sha2::Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argon2id_round_trip_with_plan_params() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(hash.starts_with("$argon2id$v=19$m=65536,t=3,p=1$"), "{hash}");
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong password entirely", &hash));
        assert!(!verify_password("anything", "not-a-phc-string"));
    }

    #[test]
    fn policy_rules() {
        assert!(password_policy_error("short", "alice").is_some());
        assert!(password_policy_error("alicealiceal", "alice").is_none());
        assert!(password_policy_error("alice", "alice").is_some());
        assert!(password_policy_error("password123", "bob").is_some());
        assert!(password_policy_error("a-unique-passphrase", "bob").is_none());
    }

    #[test]
    fn tokens_are_256_bit_and_hash_stable() {
        let t1 = generate_token();
        let t2 = generate_token();
        assert_ne!(t1, t2);
        assert!(t1.len() >= 43);
        assert_eq!(hash_token(&t1), hash_token(&t1));
        assert_ne!(hash_token(&t1), hash_token(&t2));
    }

    #[tokio::test]
    async fn store_mutate_and_reload() {
        let dir = std::env::temp_dir().join(format!(
            "manager-api-users-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = UserStore::load(&dir).await.unwrap();
        store
            .mutate::<_, StoreError>(|f| {
                f.users.push(User {
                    id: "u1".into(),
                    username: "alice".into(),
                    password_hash: hash_password("a-unique-passphrase").unwrap(),
                    role: RoleName::Admin,
                    disabled: false,
                    must_change_password: false,
                    totp_enabled: false,
                    recovery_code_hashes: vec![],
                    created_ms: now_ms(),
                    updated_ms: now_ms(),
                });
                Ok(())
            })
            .await
            .unwrap();
        assert!(dir.join(STORE_FILE).exists());
        let reloaded = UserStore::load(&dir).await.unwrap();
        let name = reloaded.read(|f| f.users[0].username.clone()).await;
        assert_eq!(name, "alice");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

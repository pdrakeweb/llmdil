//! TOTP enrollment and one-time enrollment codes for [`AuthService`].

use super::totp::recovery_code;
use super::*;

impl AuthService {
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
    /// Every enrollment — first or replacing a live factor — requires
    /// step-up: `step_up_password` must be the account password (the
    /// same step-up `totp_disable` requires). A missing or wrong
    /// password counts toward the account lockout, and a locked
    /// account is refused (SEC-04): a stolen session alone must not be
    /// able to bind a second factor only its holder knows. An account
    /// that must change its password is refused before the step-up
    /// (SEC-03).
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
        if user.must_change_password {
            return Err(AuthError::PasswordChangeRequired);
        }
        let ok = match step_up_password {
            Some(p) if !self.is_locked(&user.username.to_lowercase()).await => {
                verify_password_async(p, &user.password_hash).await
            }
            _ => false,
        };
        if !ok {
            self.record_step_up_failure(&user.username).await;
            return Err(AuthError::InvalidCredentials);
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
                    u.last_totp_step = None;
                    u.recovery_code_hashes = hashes;
                    u.updated_ms = now_ms();
                }
                Ok(())
            })
            .await?;
        Ok(codes)
    }

    /// Turn TOTP off, dropping recovery codes, any pending enrollment and the stored secret.
    /// Performs no step-up itself: callers must verify the password first
    /// ([`Self::verify_step_up`]).
    pub async fn totp_disable(&self, user_id: &str) -> Result<(), AuthError> {
        let uid = user_id.to_string();
        self.store
            .mutate::<_, AuthError>(|f| {
                if let Some(u) = f.users.iter_mut().find(|u| u.id == uid) {
                    u.totp_enabled = false;
                    u.last_totp_step = None;
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

    /// Exchange a one-time enrollment code for a new session on `device_name`. The code is
    /// consumed by any attempt that names it, even an expired one; an unknown code or a
    /// disabled user yields `InvalidCredentials`.
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

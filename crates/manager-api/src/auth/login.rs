//! Password login, the TOTP second stage and step-up checks for [`AuthService`].

use super::*;

impl AuthService {
    // -- login ----------------------------------------------------------

    /// Password stage of login. Unknown users, wrong passwords, disabled or locked accounts
    /// and IP-throttled sources all yield `InvalidCredentials` after the same argon2 cost;
    /// each failure counts toward the account lockout and the per-IP throttle. A TOTP account
    /// gets a 5-minute challenge instead of tokens, and its lockout is not reset yet.
    pub async fn login(
        &self,
        username: &str,
        password: &str,
        device_name: &str,
        source_ip: &str,
    ) -> Result<LoginOutcome, AuthError> {
        if username.len() > MAX_USERNAME_LEN {
            // No account can have this name. Same cost and answer as an
            // unknown user, but no per-account bookkeeping (SEC-05).
            let _ = verify_password_async(password, crate::users::dummy_hash()).await;
            let mut ips = self.ip_failures.lock().await;
            ips.entry(source_ip.to_string()).or_default().push(now_ms());
            return Err(AuthError::InvalidCredentials);
        }
        let key = username.to_lowercase();
        let locked = self.is_locked(&key).await;
        let throttled = self.ip_throttled(source_ip).await;
        let user = self
            .store
            .read(|f| {
                f.users
                    .iter()
                    .find(|u| u.username.eq_ignore_ascii_case(username))
                    .cloned()
            })
            .await;

        // Pay the argon2 cost in every path (dummy hash for unknown
        // users) so timing does not enumerate accounts. Off the async
        // workers (SEC-06).
        let verified = match &user {
            Some(u) => verify_password_async(password, &u.password_hash).await,
            None => {
                let _ = verify_password_async(password, crate::users::dummy_hash()).await;
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

        if user.totp_enabled {
            // The lockout counter is NOT reset here: the password is
            // only the first factor. Resetting at this stage let an
            // attacker holding the password alternate login / one TOTP
            // guess forever without ever reaching the lock threshold
            // (finding SEC-02). It resets when tokens are issued.
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
        self.record_success(&key).await;
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
        let key = user.username.to_lowercase();
        // A lock that engaged after the password stage (TOTP guesses
        // count toward it, SEC-02) also stops the second stage.
        if user.disabled || self.is_locked(&key).await {
            return Err(AuthError::InvalidCredentials);
        }
        if self.verify_user_totp(&user, code).await {
            // Re-insert nothing: the challenge is consumed on success
            // and on failure (one guess per challenge; the client
            // logs in again for another).
            self.record_success(&key).await;
            let pair = self
                .create_session(&user.id, &challenge.device_name)
                .await?;
            return Ok((pair, UserView::from(&user)));
        }
        self.record_failure(&user.username.to_lowercase(), source_ip)
            .await;
        Err(AuthError::InvalidCredentials)
    }

    /// Step-up password check for a signed-in user (TOTP disable):
    /// counts failures against the account lockout and refuses while
    /// the account is locked (SEC-04), verifying off the async workers
    /// (SEC-06).
    pub async fn verify_step_up(&self, user_id: &str, password: &str) -> bool {
        let user = self
            .store
            .read(|f| f.users.iter().find(|u| u.id == user_id).cloned())
            .await;
        let Some(user) = user else {
            return false;
        };
        if !self.is_locked(&user.username.to_lowercase()).await
            && verify_password_async(password, &user.password_hash).await
        {
            return true;
        }
        self.record_step_up_failure(&user.username).await;
        false
    }

    /// Verify a TOTP or recovery code for a user. Recovery codes are
    /// single-use and consumed here.
    pub async fn verify_user_totp(&self, user: &User, code: &str) -> bool {
        let code = code.trim();
        // Recovery codes first (they are longer than 6 digits).
        if code.len() > 6 {
            // Check and consume in one step under the store's write
            // lock, against the live record rather than the caller's
            // snapshot, so two concurrent submissions of one recovery
            // code cannot both pass (same shape as the SEC-15 TOTP
            // replay guard). A failed store write refuses the code.
            let hash = hash_token(&code.to_uppercase());
            let user_id = user.id.clone();
            return self
                .store
                .mutate::<_, AuthError>(|f| {
                    let Some(u) = f.users.iter_mut().find(|u| u.id == user_id) else {
                        return Ok(false);
                    };
                    let before = u.recovery_code_hashes.len();
                    u.recovery_code_hashes.retain(|h| h != &hash);
                    Ok(u.recovery_code_hashes.len() < before)
                })
                .await
                .unwrap_or(false);
        }
        let Some(secret_b32) = self.creds.read(&totp_target(&user.id)).await else {
            return false;
        };
        let Some(secret) = base32_decode(&secret_b32) else {
            return false;
        };
        // Replay guard (SEC-15): accept only a step after the last
        // accepted one, checked and recorded under the store's write
        // lock so two concurrent submissions of one code cannot both
        // pass. Earliest acceptable step first, so clock skew burns
        // as few future steps as possible.
        let steps = totp_matching_steps(&secret, code, now_ms() / 1000);
        if steps.is_empty() {
            return false;
        }
        let user_id = user.id.clone();
        self.store
            .mutate::<_, AuthError>(|f| {
                let Some(u) = f.users.iter_mut().find(|u| u.id == user_id) else {
                    return Ok(false);
                };
                let after = u.last_totp_step.unwrap_or(i64::MIN);
                match steps.iter().copied().find(|s| *s > after) {
                    Some(step) => {
                        u.last_totp_step = Some(step);
                        Ok(true)
                    }
                    None => Ok(false),
                }
            })
            .await
            .unwrap_or(false)
    }
}

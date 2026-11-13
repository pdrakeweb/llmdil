//! Session issue, refresh rotation, bearer validation and revocation for [`AuthService`].

use super::*;

impl AuthService {
    // -- sessions -------------------------------------------------------

    /// Mint a new per-device session. Performs no credential check of its own: callers must
    /// already have authenticated the user (password, TOTP or enrollment code). Only the
    /// token hashes are persisted. An account flagged must-change-password gets a short
    /// session ([`MUST_CHANGE_SESSION_TTL_MS`] for both tokens) instead of 30 days (SEC-03).
    pub async fn create_session(
        &self,
        user_id: &str,
        device_name: &str,
    ) -> Result<TokenPair, AuthError> {
        let access = generate_token();
        let refresh = generate_token();
        let now = now_ms();
        let uid = user_id.to_string();
        let device = device_name.to_string();
        self.store
            .mutate::<_, AuthError>(|f| {
                let must_change = f
                    .users
                    .iter()
                    .find(|u| u.id == uid)
                    .map(|u| u.must_change_password)
                    .unwrap_or(false);
                let (access_ttl, refresh_ttl) = if must_change {
                    (
                        ACCESS_TTL_MS.min(MUST_CHANGE_SESSION_TTL_MS),
                        MUST_CHANGE_SESSION_TTL_MS,
                    )
                } else {
                    (ACCESS_TTL_MS, REFRESH_TTL_MS)
                };
                let session = Session {
                    id: uuid::Uuid::new_v4().to_string(),
                    user_id: uid,
                    device_name: device,
                    access_hash: hash_token(&access),
                    refresh_hash: hash_token(&refresh),
                    retired_refresh_hashes: Vec::new(),
                    retired_refresh_ms: Vec::new(),
                    created_ms: now,
                    access_expires_ms: now + access_ttl,
                    refresh_expires_ms: now + refresh_ttl,
                    last_used_ms: now,
                    revoked: false,
                };
                let pair = TokenPair {
                    access_token: access.clone(),
                    refresh_token: refresh.clone(),
                    session_id: session.id.clone(),
                    access_expires_ms: session.access_expires_ms,
                    refresh_expires_ms: session.refresh_expires_ms,
                };
                f.sessions.push(session);
                Ok(pair)
            })
            .await
    }

    /// Rotate a refresh token. Presenting a retired token revokes
    /// the whole session family (theft detection). A disabled account
    /// is refused, and an account flagged must-change-password gets
    /// `PasswordChangeRequired` without rotating (SEC-03): refresh is a
    /// public route, so the middleware gate never sees it.
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
            .mutate::<Result<TokenPair, AuthError>, AuthError>(|f| {
                let Some(s) = f.sessions.iter_mut().find(|s| s.id == session_id) else {
                    return Ok(Err(AuthError::InvalidCredentials));
                };
                if s.revoked || s.refresh_expires_ms <= now {
                    return Ok(Err(AuthError::InvalidCredentials));
                }
                // Re-check under the write lock (finding SEC-12,
                // docs/code-review-v0.1.60.md): the lookup above ran
                // under a read lock, so a concurrent refresh with the
                // same token may have rotated it since. That is reuse
                // of a now-retired token — revoke the family, exactly
                // as the read-side reuse check does, instead of letting
                // both callers rotate successfully.
                if s.refresh_hash != hash {
                    s.revoked = true;
                    return Ok(Err(AuthError::SessionRevoked));
                }
                // The owner's live record decides, under the same lock.
                match f.users.iter().find(|u| u.id == s.user_id) {
                    Some(u) if u.disabled => return Ok(Err(AuthError::InvalidCredentials)),
                    Some(u) if u.must_change_password => {
                        return Ok(Err(AuthError::PasswordChangeRequired))
                    }
                    Some(_) => {}
                    None => return Ok(Err(AuthError::InvalidCredentials)),
                }
                s.retire_refresh_hash(now);
                s.access_hash = hash_token(&access);
                s.refresh_hash = hash_token(&refresh);
                s.access_expires_ms = now + ACCESS_TTL_MS;
                s.refresh_expires_ms = now + REFRESH_TTL_MS;
                s.last_used_ms = now;
                Ok(Ok(TokenPair {
                    access_token: access.clone(),
                    refresh_token: refresh.clone(),
                    session_id: s.id.clone(),
                    access_expires_ms: s.access_expires_ms,
                    refresh_expires_ms: s.refresh_expires_ms,
                }))
            })
            .await??;
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

    /// The username owning the session a (current or retired) refresh
    /// token belongs to, for auditing refused refreshes. `None` when
    /// the token matches no session.
    pub async fn refresh_token_owner(&self, refresh_token: &str) -> Option<String> {
        let hash = hash_token(refresh_token);
        self.store
            .read(|f| {
                let s = f.sessions.iter().find(|s| {
                    s.refresh_hash == hash || s.retired_refresh_hashes.iter().any(|h| h == &hash)
                })?;
                f.users
                    .iter()
                    .find(|u| u.id == s.user_id)
                    .map(|u| u.username.clone())
            })
            .await
    }

    /// Validate a bearer credential (session access token or API
    /// token) into a principal. Records last use.
    pub async fn validate_bearer(&self, token: &str) -> Option<Principal> {
        let hash = hash_token(token);
        let now = now_ms();
        // Session access token?
        let session_user =
            self.store
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
                must_change_password: user.must_change_password,
            });
        }
        // API token?
        let api = self
            .store
            .read(|f| {
                let t = f.api_tokens.iter().find(|t| {
                    t.hash == hash && !t.revoked && t.expires_ms.map(|e| e > now).unwrap_or(true)
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
                must_change_password: user.must_change_password,
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
                    .map(|s| now - s.last_used_ms > LAST_USED_WRITE_INTERVAL_MS)
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

    /// Record API-token use, throttled like [`Self::touch_session`]
    /// (SEC-14): `admin-users.json` is rewritten at most once per
    /// [`LAST_USED_WRITE_INTERVAL_MS`] per token, not on every request.
    async fn touch_api_token(&self, token_id: &str) {
        let now = now_ms();
        let stale = self
            .store
            .read(|f| {
                f.api_tokens
                    .iter()
                    .find(|t| t.id == token_id)
                    .map(|t| {
                        t.last_used_ms
                            .map(|last| now - last > LAST_USED_WRITE_INTERVAL_MS)
                            .unwrap_or(true)
                    })
                    .unwrap_or(false)
            })
            .await;
        if !stale {
            return;
        }
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

    /// Mark a session revoked; its access and refresh tokens stop validating at once. An
    /// unknown id is a silent no-op, so callers scoping by owner must check ownership first.
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

    /// Revoke every session of `user_id` except `except` (the caller's own, on password
    /// change). API tokens are untouched. A store write failure is returned rather than
    /// swallowed: the in-memory revocation has taken effect, but the caller must not report
    /// success for a revocation that may not survive a restart.
    pub async fn revoke_user_sessions(
        &self,
        user_id: &str,
        except: Option<&str>,
    ) -> Result<(), AuthError> {
        let uid = user_id.to_string();
        let except = except.map(str::to_string);
        self.store
            .mutate::<_, AuthError>(|f| {
                for s in f.sessions.iter_mut() {
                    if s.user_id == uid && except.as_deref() != Some(&s.id) {
                        s.revoked = true;
                    }
                }
                Ok(())
            })
            .await
    }
}

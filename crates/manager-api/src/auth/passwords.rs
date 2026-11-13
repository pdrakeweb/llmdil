//! Password change and admin password reset for [`AuthService`].

use super::*;

impl AuthService {
    // -- passwords ------------------------------------------------------

    /// Self-service password change. The current password is a step-up check counted
    /// against the account lockout (SEC-04); the new one must pass [`password_policy_error`].
    /// On success clears must-change and revokes every other session, keeping `keep_session`.
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
        if self.is_locked(&user.username.to_lowercase()).await
            || !verify_password_async(current, &user.password_hash).await
        {
            self.record_step_up_failure(&user.username).await;
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
        self.revoke_user_sessions(user_id, keep_session).await
    }

    /// Admin reset: a generated temporary password, must-change set,
    /// and every session AND API token of the user revoked (SEC-03) —
    /// in one store write, so a failed write reaches the admin instead
    /// of a reported reset whose revocations were lost. Returns the
    /// temporary password (shown once to the admin).
    pub async fn admin_reset_password(&self, user_id: &str) -> Result<String, AuthError> {
        let temp = generate_token()[..16].to_string();
        let hash = hash_password(&temp).map_err(|_| AuthError::Internal)?;
        let uid = user_id.to_string();
        let found = self
            .store
            .mutate::<bool, AuthError>(|f| {
                let Some(u) = f.users.iter_mut().find(|u| u.id == uid) else {
                    return Ok(false);
                };
                u.password_hash = hash;
                u.must_change_password = true;
                u.updated_ms = now_ms();
                for s in f.sessions.iter_mut().filter(|s| s.user_id == uid) {
                    s.revoked = true;
                }
                for t in f.api_tokens.iter_mut().filter(|t| t.user_id == uid) {
                    t.revoked = true;
                }
                Ok(true)
            })
            .await?;
        if !found {
            return Err(AuthError::NotFound("user".into()));
        }
        Ok(temp)
    }
}

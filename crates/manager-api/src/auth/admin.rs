//! User administration (create, update, delete) for [`AuthService`].

use super::*;

impl AuthService {
    // -- user administration ---------------------------------------------

    /// Create an enabled account without TOTP or a must-change flag. The username is trimmed,
    /// must be 1..=[`MAX_USERNAME_LEN`] bytes and unique ignoring ASCII case; the password
    /// must pass the policy.
    pub async fn create_user(
        &self,
        username: &str,
        password: &str,
        role: RoleName,
    ) -> Result<UserView, AuthError> {
        let username = username.trim().to_string();
        if username.is_empty() || username.len() > 64 {
            return Err(AuthError::InvalidInput(
                "username is empty or too long".into(),
            ));
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
            last_totp_step: None,
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

    /// Change a user's role and/or disabled flag. Demoting or disabling the last enabled
    /// admin is refused. Any role change or disable revokes all the user's sessions at once,
    /// and a disabled user's API tokens stop validating too.
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
            self.revoke_user_sessions(user_id, None).await?;
        }
        Ok(view)
    }

    /// Delete a user with their sessions, API tokens and TOTP secret. Refused for
    /// self-deletion and for the last enabled admin.
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
}

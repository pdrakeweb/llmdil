//! Admin-issued API tokens for [`AuthService`].

use super::*;

impl AuthService {
    // -- API tokens --------------------------------------------------------

    /// Issue an API token for an existing, enabled user, capped at operator role (viewer or
    /// operator only). Returns the metadata and the token value, which is never retrievable
    /// again — only its hash is stored. `expires_days = None` never expires.
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

    /// Revoke an API token by id; it stops validating on the next request. Permanent: there
    /// is no un-revoke.
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
}

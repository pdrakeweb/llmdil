//! Unit tests for [`AuthService`].

use super::totp::recovery_code;
use super::*;
use crate::users::UserStore;

async fn service() -> Arc<AuthService> {
    AuthService::new(
        UserStore::in_memory(),
        Arc::new(MemoryCredentialStore::default()),
    )
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

/// Enable TOTP for `user_id` with a fixed secret; returns the
/// secret bytes.
async fn enable_totp(svc: &AuthService, user_id: &str) -> Vec<u8> {
    let secret = b"0123456789abcdefghij".to_vec();
    svc.creds
        .write(&totp_target(user_id), &base32_encode(&secret))
        .await
        .unwrap();
    let uid = user_id.to_string();
    svc.store
        .mutate::<_, AuthError>(|f| {
            f.users
                .iter_mut()
                .find(|u| u.id == uid)
                .unwrap()
                .totp_enabled = true;
            Ok(())
        })
        .await
        .unwrap();
    secret
}

/// SEC-12: concurrent refreshes with the same refresh token yield
/// at most one rotation. (The interleaving is not forced, so this
/// can only expose the race, never fail spuriously.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_refresh_rotates_at_most_once() {
    let svc = service().await;
    let user = svc
        .create_user("frank", "a-unique-passphrase", RoleName::Operator)
        .await
        .unwrap();
    for _ in 0..20 {
        let pair = svc.create_session(&user.id, "phone").await.unwrap();
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let svc = svc.clone();
                let token = pair.refresh_token.clone();
                tokio::spawn(async move { svc.refresh(&token).await.is_ok() })
            })
            .collect();
        let mut ok = 0;
        for t in tasks {
            if t.await.unwrap() {
                ok += 1;
            }
        }
        assert!(ok <= 1, "{ok} concurrent rotations of one refresh token");
    }
}

/// SEC-02 (docs/code-review-v0.1.60.md): a correct password must
/// not reset the lockout while the second factor is still pending,
/// so TOTP guessing reaches the lock threshold.
#[tokio::test]
async fn totp_guessing_locks_the_account() {
    let svc = service().await;
    let user = svc
        .create_user("carol", "a-unique-passphrase", RoleName::Operator)
        .await
        .unwrap();
    let secret = enable_totp(&svc, &user.id).await;
    let now = now_ms() / 1000;
    let valid: Vec<u32> = (-1..=1).map(|d| totp_at(&secret, now + d * 30)).collect();
    let wrong = (0..1_000_000u32).find(|c| !valid.contains(c)).unwrap();
    let wrong = format!("{wrong:06}");
    for _ in 0..LOCK_THRESHOLD {
        let LoginOutcome::TotpRequired { challenge_id } = svc
            .login("carol", "a-unique-passphrase", "phone", "10.0.0.1")
            .await
            .unwrap()
        else {
            panic!("expected a TOTP challenge");
        };
        let err = svc
            .complete_totp_login(&challenge_id, &wrong, "10.0.0.1")
            .await
            .unwrap_err();
        assert_eq!(err, AuthError::InvalidCredentials);
    }
    // Locked: even the correct password is refused.
    let err = svc
        .login("carol", "a-unique-passphrase", "phone", "10.0.0.2")
        .await
        .unwrap_err();
    assert_eq!(err, AuthError::InvalidCredentials);
}

/// SEC-02: a successful second factor clears the counter, and a
/// lock engaged after the password stage stops the TOTP stage.
#[tokio::test]
async fn totp_success_resets_and_lock_blocks_second_stage() {
    let svc = service().await;
    let user = svc
        .create_user("dave", "a-unique-passphrase", RoleName::Operator)
        .await
        .unwrap();
    let secret = enable_totp(&svc, &user.id).await;
    let LoginOutcome::TotpRequired { challenge_id } = svc
        .login("dave", "a-unique-passphrase", "phone", "10.0.0.1")
        .await
        .unwrap()
    else {
        panic!("expected a TOTP challenge");
    };
    // Lock the account between the stages.
    for _ in 0..LOCK_THRESHOLD {
        svc.record_account_failure("dave").await;
    }
    let code = format!("{:06}", totp_at(&secret, now_ms() / 1000));
    let err = svc
        .complete_totp_login(&challenge_id, &code, "10.0.0.1")
        .await
        .unwrap_err();
    assert_eq!(err, AuthError::InvalidCredentials);
    // Unlocked: the full flow succeeds and clears the record.
    svc.lockouts.lock().await.clear();
    svc.record_account_failure("dave").await;
    let LoginOutcome::TotpRequired { challenge_id } = svc
        .login("dave", "a-unique-passphrase", "phone", "10.0.0.1")
        .await
        .unwrap()
    else {
        panic!("expected a TOTP challenge");
    };
    let code = format!("{:06}", totp_at(&secret, now_ms() / 1000));
    svc.complete_totp_login(&challenge_id, &code, "10.0.0.1")
        .await
        .unwrap();
    assert!(!svc.lockouts.lock().await.contains_key("dave"));
}

/// SEC-05: oversized usernames are refused before any per-account
/// bookkeeping, and stale lockout entries are pruned.
#[tokio::test]
async fn oversized_usernames_do_not_grow_the_lockout_map() {
    let svc = service().await;
    for i in 0..3 {
        let name = format!("{i}{}", "x".repeat(10_000));
        let err = svc
            .login(&name, "whatever-password", "phone", "10.0.0.9")
            .await
            .unwrap_err();
        assert_eq!(err, AuthError::InvalidCredentials);
    }
    assert!(svc.lockouts.lock().await.is_empty());
    // An expired entry is pruned on the next failure.
    svc.lockouts.lock().await.insert(
        "stale".to_string(),
        LockState {
            failures: vec![now_ms() - LOCK_WINDOW_MS - 1],
            ..Default::default()
        },
    );
    svc.record_account_failure("fresh").await;
    let map = svc.lockouts.lock().await;
    assert!(!map.contains_key("stale"));
    assert!(map.contains_key("fresh"));
}

/// SEC-04: wrong current-password guesses on the change-password
/// path count toward the account lockout.
#[tokio::test]
async fn change_password_guesses_lock_the_account() {
    let svc = service().await;
    let user = svc
        .create_user("erin", "a-unique-passphrase", RoleName::Operator)
        .await
        .unwrap();
    for _ in 0..LOCK_THRESHOLD {
        let err = svc
            .change_password(&user.id, "not-the-password", "another-unique-phrase", None)
            .await
            .unwrap_err();
        assert_eq!(err, AuthError::InvalidCredentials);
    }
    assert!(svc.is_locked("erin").await);
    // While locked, even the right current password is refused.
    let err = svc
        .change_password(
            &user.id,
            "a-unique-passphrase",
            "another-unique-phrase",
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err, AuthError::InvalidCredentials);
    assert!(!svc.verify_step_up(&user.id, "a-unique-passphrase").await);
}

#[tokio::test]
async fn admin_guards() {
    let svc = service().await;
    let admin = svc
        .create_user("root", "a-unique-passphrase", RoleName::Admin)
        .await
        .unwrap();
    // Last enabled admin cannot be disabled or demoted.
    assert!(svc.update_user(&admin.id, None, Some(true)).await.is_err());
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
        .totp_begin_enrollment(&user.id, "carol", Some("a-unique-passphrase"))
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
        .totp_begin_enrollment(&user.id, "erin", Some("a-unique-passphrase"))
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

/// SEC-04: first enrollment needs the password step-up too; a
/// missing or wrong password counts toward the lockout, and a
/// must-change account is refused before any step-up.
#[tokio::test]
async fn first_totp_enrollment_requires_step_up() {
    let svc = service().await;
    let user = svc
        .create_user("gwen", "a-unique-passphrase", RoleName::Operator)
        .await
        .unwrap();
    assert_eq!(
        svc.totp_begin_enrollment(&user.id, "gwen", None)
            .await
            .unwrap_err(),
        AuthError::InvalidCredentials
    );
    assert_eq!(svc.lockouts.lock().await["gwen"].failures.len(), 1);
    for _ in 1..LOCK_THRESHOLD {
        assert_eq!(
            svc.totp_begin_enrollment(&user.id, "gwen", Some("not-the-password"))
                .await
                .unwrap_err(),
            AuthError::InvalidCredentials
        );
    }
    assert!(svc.is_locked("gwen").await);
    // Locked: even the right password is refused, and nothing is pending.
    assert!(svc
        .totp_begin_enrollment(&user.id, "gwen", Some("a-unique-passphrase"))
        .await
        .is_err());
    assert!(!svc.pending_totp.lock().await.contains_key(&user.id));
    svc.lockouts.lock().await.clear();
    svc.totp_begin_enrollment(&user.id, "gwen", Some("a-unique-passphrase"))
        .await
        .unwrap();
    assert!(svc.pending_totp.lock().await.contains_key(&user.id));

    // A must-change account gets PasswordChangeRequired first, and
    // the attempt is not a step-up failure.
    let flagged = svc
        .create_user("hal", "a-unique-passphrase", RoleName::Operator)
        .await
        .unwrap();
    svc.admin_reset_password(&flagged.id).await.unwrap();
    assert_eq!(
        svc.totp_begin_enrollment(&flagged.id, "hal", Some("whatever"))
            .await
            .unwrap_err(),
        AuthError::PasswordChangeRequired
    );
    assert!(!svc.lockouts.lock().await.contains_key("hal"));
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
        .totp_begin_enrollment(&user.id, "frank", Some("a-unique-passphrase"))
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
        must_change_password: false,
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
    let (pair, view) = svc.redeem_enrollment_code(&code, "pixel").await.unwrap();
    assert_eq!(view.username, "dave");
    assert!(svc.validate_bearer(&pair.access_token).await.is_some());
    assert!(svc.redeem_enrollment_code(&code, "pixel").await.is_err());
}

/// SEC-15: an accepted TOTP code cannot be replayed within the
/// ±1-step window, nor can a code for an earlier step; the guard
/// is persisted on the user and reset by re-enrollment.
#[tokio::test]
async fn totp_codes_cannot_be_replayed() {
    let svc = service().await;
    let user = svc
        .create_user("ivy", "a-unique-passphrase", RoleName::Operator)
        .await
        .unwrap();
    let secret = enable_totp(&svc, &user.id).await;
    let now = now_ms() / 1000;
    let step = now.div_euclid(30);
    let fetch = |svc: Arc<AuthService>, id: String| async move {
        svc.store
            .read(|f| f.users.iter().find(|u| u.id == id).cloned().unwrap())
            .await
    };
    let current = format!("{:06}", totp_at(&secret, now));
    let stored = fetch(svc.clone(), user.id.clone()).await;
    assert!(svc.verify_user_totp(&stored, &current).await);
    let stored = fetch(svc.clone(), user.id.clone()).await;
    assert!(stored.last_totp_step.unwrap() >= step);
    // Same code again: refused.
    assert!(!svc.verify_user_totp(&stored, &current).await);
    // The previous step's code (still inside the window): refused.
    let previous = format!("{:06}", totp_at(&secret, now - 30));
    if previous != current {
        assert!(!svc.verify_user_totp(&stored, &previous).await);
    }
    // The next step's code is still acceptable once.
    let next = format!("{:06}", totp_at(&secret, now + 30));
    if next != current {
        assert!(svc.verify_user_totp(&stored, &next).await);
        assert!(!svc.verify_user_totp(&stored, &next).await);
    }
    // Over the login flow: a replayed code fails the second stage.
    svc.store
        .mutate::<_, AuthError>(|f| {
            f.users[0].last_totp_step = None;
            Ok(())
        })
        .await
        .unwrap();
    for expect_ok in [true, false] {
        let LoginOutcome::TotpRequired { challenge_id } = svc
            .login("ivy", "a-unique-passphrase", "phone", "10.0.0.1")
            .await
            .unwrap()
        else {
            panic!("expected a TOTP challenge");
        };
        let res = svc
            .complete_totp_login(&challenge_id, &current, "10.0.0.1")
            .await;
        assert_eq!(res.is_ok(), expect_ok);
    }
    // Disabling TOTP clears the guard.
    svc.totp_disable(&user.id).await.unwrap();
    assert!(fetch(svc.clone(), user.id.clone())
        .await
        .last_totp_step
        .is_none());
}

/// A recovery code is consumed atomically: concurrent uses of one
/// code against the same (pre-use) snapshot accept exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_recovery_code_use_accepts_once() {
    let svc = service().await;
    let user = svc
        .create_user("rhea", "a-unique-passphrase", RoleName::Operator)
        .await
        .unwrap();
    for round in 0..20 {
        let code = recovery_code();
        let uid = user.id.clone();
        let hash = hash_token(&code.to_uppercase());
        svc.store
            .mutate::<_, AuthError>(|f| {
                let u = f.users.iter_mut().find(|u| u.id == uid).unwrap();
                u.totp_enabled = true;
                u.recovery_code_hashes = vec![hash, "other".into()];
                Ok(())
            })
            .await
            .unwrap();
        // Every task checks against the same snapshot, taken while
        // the code is still unused (the old check-then-act read it).
        let snapshot = svc
            .store
            .read(|f| f.users.iter().find(|u| u.id == user.id).cloned().unwrap())
            .await;
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let svc = svc.clone();
                let snapshot = snapshot.clone();
                let code = code.clone();
                tokio::spawn(async move { svc.verify_user_totp(&snapshot, &code).await })
            })
            .collect();
        let mut accepted = 0;
        for t in tasks {
            if t.await.unwrap() {
                accepted += 1;
            }
        }
        assert_eq!(accepted, 1, "round {round}: {accepted} acceptances");
        let left = svc
            .store
            .read(|f| {
                f.users
                    .iter()
                    .find(|u| u.id == user.id)
                    .unwrap()
                    .recovery_code_hashes
                    .clone()
            })
            .await;
        assert_eq!(left, vec!["other".to_string()]);
    }
}

/// SEC-15: a user record written before `last_totp_step` existed
/// still loads, with no replay state.
#[test]
fn user_without_last_totp_step_deserializes() {
    let json = r#"{"id":"u","username":"a","password_hash":"h","role":"viewer","created_ms":0,"updated_ms":0}"#;
    let u: User = serde_json::from_str(json).unwrap();
    assert!(u.last_totp_step.is_none());
}

/// SEC-14: API-token use updates `last_used_ms` at most once per
/// interval, so a busy token does not rewrite the store per request.
#[tokio::test]
async fn api_token_last_used_is_throttled() {
    let svc = service().await;
    let admin = svc
        .create_user("root", "a-unique-passphrase", RoleName::Admin)
        .await
        .unwrap();
    let issuer = Principal {
        user_id: admin.id.clone(),
        username: "root".into(),
        role: Role::Admin,
        auth_type: AuthType::Session,
        session_id: "s".into(),
        must_change_password: false,
    };
    let (meta, value) = svc
        .create_api_token(&issuer, &admin.id, "ci", RoleName::Viewer, None)
        .await
        .unwrap();
    let last_used = |svc: Arc<AuthService>, id: String| async move {
        svc.store
            .read(|f| {
                f.api_tokens
                    .iter()
                    .find(|t| t.id == id)
                    .unwrap()
                    .last_used_ms
            })
            .await
    };
    // First use is recorded.
    svc.validate_bearer(&value).await.unwrap();
    assert!(last_used(svc.clone(), meta.id.clone()).await.is_some());
    // A recent use: no rewrite.
    let recent = now_ms() - LAST_USED_WRITE_INTERVAL_MS / 2;
    let set = |svc: Arc<AuthService>, id: String, at: i64| async move {
        svc.store
            .mutate::<_, AuthError>(|f| {
                f.api_tokens
                    .iter_mut()
                    .find(|t| t.id == id)
                    .unwrap()
                    .last_used_ms = Some(at);
                Ok(())
            })
            .await
            .unwrap();
    };
    set(svc.clone(), meta.id.clone(), recent).await;
    svc.validate_bearer(&value).await.unwrap();
    assert_eq!(last_used(svc.clone(), meta.id.clone()).await, Some(recent));
    // A stale use: refreshed.
    let stale = now_ms() - LAST_USED_WRITE_INTERVAL_MS - 1_000;
    set(svc.clone(), meta.id.clone(), stale).await;
    svc.validate_bearer(&value).await.unwrap();
    assert!(last_used(svc.clone(), meta.id.clone()).await.unwrap() > stale);
}

/// SEC-14: refresh rotation records when each hash was retired.
#[tokio::test]
async fn refresh_rotation_records_retirement_times() {
    let svc = service().await;
    let user = svc
        .create_user("hank", "a-unique-passphrase", RoleName::Operator)
        .await
        .unwrap();
    let pair = svc.create_session(&user.id, "phone").await.unwrap();
    let (pair2, _) = svc.refresh(&pair.refresh_token).await.unwrap();
    svc.refresh(&pair2.refresh_token).await.unwrap();
    let (hashes, times) = svc
        .store
        .read(|f| {
            let s = &f.sessions[0];
            (
                s.retired_refresh_hashes.clone(),
                s.retired_refresh_ms.clone(),
            )
        })
        .await;
    assert_eq!(hashes.len(), 2);
    assert_eq!(times.len(), 2);
    // Reuse detection still works on a recently retired token.
    assert_eq!(
        svc.refresh(&pair.refresh_token).await.unwrap_err(),
        AuthError::SessionRevoked
    );
}

/// SEC-13: a lock engaging is audited (login path with the source
/// IP, step-up path too); failures below the threshold are not
/// lock events.
#[tokio::test]
async fn lockout_events_are_audited() {
    let dir = std::env::temp_dir().join(format!(
        "manager-api-lock-audit-{}-{}",
        std::process::id(),
        generate_token()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let log = Arc::new(crate::audit::AuditLog::new(&dir));
    let svc = service().await;
    svc.attach_audit(log.clone());
    for _ in 0..LOCK_THRESHOLD - 1 {
        svc.record_failure("mallory-target", "10.1.2.3").await;
    }
    assert!(log.read_recent(0, 10).is_empty());
    svc.record_failure("mallory-target", "10.1.2.3").await;
    let user = svc
        .create_user("gina", "a-unique-passphrase", RoleName::Operator)
        .await
        .unwrap();
    for _ in 0..LOCK_THRESHOLD {
        assert!(!svc.verify_step_up(&user.id, "not-the-password").await);
    }
    let entries = log.read_recent(0, 10);
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!(entries[0]["action"], "account_locked");
    assert_eq!(entries[0]["actor"], "gina");
    assert_eq!(entries[1]["action"], "account_locked");
    assert_eq!(entries[1]["actor"], "mallory-target");
    assert_eq!(entries[1]["source_ip"], "10.1.2.3");
    assert_eq!(
        entries[1]["result"],
        format!("locked_for_{}s", LOCK_BASE_MS / 1000)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

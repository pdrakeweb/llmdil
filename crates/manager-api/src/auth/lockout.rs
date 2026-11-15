//! Account lockout and per-IP throttle bookkeeping for [`AuthService`].

use super::*;

impl AuthService {
    // -- lockout bookkeeping -------------------------------------------

    pub(super) async fn is_locked(&self, username_key: &str) -> bool {
        let guard = self.lockouts.lock().await;
        guard
            .get(username_key)
            .map(|s| s.locked_until_ms > now_ms())
            .unwrap_or(false)
    }

    pub(super) async fn record_failure(&self, username_key: &str, source_ip: &str) {
        let now = now_ms();
        if let Some(locked_for) = self.record_account_failure(username_key).await {
            self.audit_lock(username_key, source_ip, locked_for);
        }
        let mut ips = self.ip_failures.lock().await;
        ips.retain(|ip, v| ip == source_ip || v.iter().any(|t| now - t < LOCK_WINDOW_MS));
        let v = ips.entry(source_ip.to_string()).or_default();
        v.retain(|t| now - t < LOCK_WINDOW_MS);
        v.push(now);
    }

    /// Count one failure against the account; returns the lock
    /// duration (ms) when this failure engaged a lock.
    pub(super) async fn record_account_failure(&self, username_key: &str) -> Option<i64> {
        let now = now_ms();
        {
            let mut guard = self.lockouts.lock().await;
            // Prune entries with no live failures and no live lock so
            // the map stays bounded by recent activity (SEC-05). The
            // escalation counter of a pruned entry restarts, which only
            // happens after a full quiet window with no lock in force.
            guard.retain(|k, st| {
                k == username_key
                    || st.locked_until_ms > now
                    || st.failures.iter().any(|t| now - t < LOCK_WINDOW_MS)
            });
            let st = guard.entry(username_key.to_string()).or_default();
            st.failures.retain(|t| now - t < LOCK_WINDOW_MS);
            st.failures.push(now);
            if st.failures.len() >= LOCK_THRESHOLD {
                let backoff = (LOCK_BASE_MS << st.lock_count.min(2)).min(LOCK_MAX_MS);
                st.locked_until_ms = now + backoff;
                st.lock_count += 1;
                st.failures.clear();
                return Some(backoff);
            }
        }
        None
    }

    /// Record a failed step-up password check (password change, TOTP
    /// disable, TOTP enrollment) against the account lockout
    /// (finding SEC-04): these
    /// endpoints otherwise offered unlimited password guessing to any
    /// holder of a bearer token.
    pub(super) async fn record_step_up_failure(&self, username: &str) {
        let key = username.to_lowercase();
        if let Some(locked_for) = self.record_account_failure(&key).await {
            // Step-ups come from an authenticated principal; the
            // service layer does not see its source address.
            self.audit_lock(&key, "unknown", locked_for);
        }
    }

    pub(super) async fn ip_throttled(&self, source_ip: &str) -> bool {
        let now = now_ms();
        let guard = self.ip_failures.lock().await;
        guard
            .get(source_ip)
            .map(|v| v.iter().filter(|t| now - *t < LOCK_WINDOW_MS).count() >= IP_FAILURE_LIMIT)
            .unwrap_or(false)
    }

    pub(super) async fn record_success(&self, username_key: &str) {
        let mut guard = self.lockouts.lock().await;
        guard.remove(username_key);
    }
}

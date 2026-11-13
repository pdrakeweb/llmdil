import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";

// ---------------------------------------------------------------------------
// Admin API (Remote Access settings): desktop-local commands served
// by the shell (src-tauri/src/main.rs), not registry operations.
// ---------------------------------------------------------------------------

export interface AdminApiStatus {
  enabled: boolean;
  running: boolean;
  port: number;
  /** The address the listener has bound (or would bind if started
   *  now): 127.0.0.1, 0.0.0.0 (all interfaces) or one interface. */
  bind: string;
  /** Configured bind choice (auth-plan §5): loopback | all |
   *  interface. Optional: older shells do not send it. */
  bind_mode?: string;
  /** The configured interface address (kept across mode changes). */
  bind_ip?: string | null;
  /** Why the listener fell back to loopback from the configured
   *  interface address (never widened); null when it did not. */
  bind_fallback?: string | null;
  /** This machine's current interface addresses, for the selector. */
  bind_candidates?: string[];
  /** False when the bind is loopback-only: no inbound rule needed. */
  firewall_needed?: boolean;
  base_url: string | null;
  cert_sha256: string | null;
  /** Why the listener's certificate is the one in use (SEC-08):
   *  loaded | generated_missing | regenerated_key_mismatch |
   *  regenerated_expiring | regenerated_corrupt |
   *  regenerated_by_operator; null before the API first starts.
   *  Optional: older shells do not send it. */
  cert_source?: string | null;
  /** What was wrong when the shell regenerated on its own. */
  cert_detail?: string | null;
  cert_not_after_ms?: number | null;
  /** Informational: current machine addresses the certificate does
   *  not list. Never acted on automatically — pinned clients ignore
   *  SANs; only Regenerate (which forces a re-pair) updates them. */
  san_drift?: boolean;
  san_drift_ips?: string[];
  firewall: string;
  note: string | null;
  user_count: number;
  admin_count: number;
}

export interface AdminApiUser {
  id: string;
  username: string;
  role: string;
  disabled: boolean;
  must_change_password: boolean;
  totp_enabled: boolean;
  created_ms: number;
}

const emptyStatus: AdminApiStatus = {
  enabled: false, running: false, port: 47821, bind: "0.0.0.0",
  bind_mode: "all", bind_ip: null, bind_fallback: null, bind_candidates: [], firewall_needed: true,
  base_url: null, cert_sha256: null, cert_source: null, cert_detail: null,
  cert_not_after_ms: null, san_drift: false, san_drift_ips: [], firewall: "unsupported",
  note: null, user_count: 0, admin_count: 0,
};

export async function adminApiGetStatus(): Promise<CallResult<AdminApiStatus>> {
  if (!isTauri()) return { data: emptyStatus, live: false };
  try {
    const data = await invoke<AdminApiStatus>("admin_api_get_status");
    return { data, live: true };
  } catch (e) {
    return { data: emptyStatus, live: true, error: errOf(e) };
  }
}

export async function adminApiSetEnabled(enabled: boolean): Promise<CallResult<AdminApiStatus>> {
  if (!isTauri()) return { data: emptyStatus, live: false, error: "admin_api_set_enabled is not available in sample mode" };
  try {
    const data = await invoke<AdminApiStatus>("admin_api_set_enabled", { enabled });
    return { data, live: true };
  } catch (e) {
    return { data: emptyStatus, live: true, error: errOf(e) };
  }
}

export async function adminApiListUsers(): Promise<CallResult<AdminApiUser[]>> {
  if (!isTauri()) return { data: [], live: false };
  try {
    const data = await invoke<{ users: AdminApiUser[] }>("admin_api_list_users");
    return { data: data.users, live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}

export async function adminApiCreateUser(username: string, password: string, role: string): Promise<CallResult<AdminApiUser | null>> {
  if (!isTauri()) return { data: null, live: false, error: "admin_api_create_user is not available in sample mode" };
  try {
    const data = await invoke<AdminApiUser>("admin_api_create_user", { username, password, role });
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

export async function adminApiUpdateUser(id: string, role?: string, disabled?: boolean): Promise<CallResult<AdminApiUser | null>> {
  if (!isTauri()) return { data: null, live: false, error: "admin_api_update_user is not available in sample mode" };
  try {
    const data = await invoke<AdminApiUser>("admin_api_update_user", { id, role: role ?? null, disabled: disabled ?? null });
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

export async function adminApiDeleteUser(id: string): Promise<CallResult<boolean>> {
  if (!isTauri()) return { data: false, live: false, error: "admin_api_delete_user is not available in sample mode" };
  try {
    await invoke<unknown>("admin_api_delete_user", { id });
    return { data: true, live: true };
  } catch (e) {
    return { data: false, live: true, error: errOf(e) };
  }
}

export async function adminApiResetPassword(id: string): Promise<CallResult<string | null>> {
  if (!isTauri()) return { data: null, live: false, error: "admin_api_reset_password is not available in sample mode" };
  try {
    const data = await invoke<{ temporary_password: string }>("admin_api_reset_password", { id });
    return { data: data.temporary_password, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

export interface AdminApiPairing {
  base_url: string;
  cert_sha256: string | null;
  enrollment_code?: string;
  /** The pairing QR (PLAN.md §3.8) as an inline <svg>, rendered by the
   *  shell (manager-api pairing module) from exactly the values above
   *  ({ base_url, cert_sha256, enrollment_code } JSON). Null without a
   *  certificate fingerprint; absent from older shells. */
  qr_svg?: string | null;
}

export async function adminApiCreateEnrollment(userId: string): Promise<CallResult<AdminApiPairing | null>> {
  if (!isTauri()) return { data: null, live: false, error: "admin_api_create_enrollment is not available in sample mode" };
  try {
    const data = await invoke<AdminApiPairing>("admin_api_create_enrollment", { userId });
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

export interface AdminApiAuditEntry {
  ts_ms: number;
  actor: string;
  auth_type: string;
  source_ip: string;
  action: string;
  target: string | null;
  result: string;
  request_id: string;
}

export async function adminApiRegenerateCert(): Promise<CallResult<AdminApiStatus>> {
  if (!isTauri()) return { data: emptyStatus, live: false, error: "admin_api_regenerate_cert is not available in sample mode" };
  try {
    const data = await invoke<AdminApiStatus>("admin_api_regenerate_cert");
    return { data, live: true };
  } catch (e) {
    return { data: emptyStatus, live: true, error: errOf(e) };
  }
}

export async function adminApiApplyFirewallRule(): Promise<CallResult<AdminApiStatus>> {
  if (!isTauri()) return { data: emptyStatus, live: false, error: "admin_api_apply_firewall_rule is not available in sample mode" };
  try {
    const data = await invoke<AdminApiStatus>("admin_api_apply_firewall_rule");
    return { data, live: true };
  } catch (e) {
    return { data: emptyStatus, live: true, error: errOf(e) };
  }
}

export async function adminApiGetAudit(limit = 20): Promise<CallResult<AdminApiAuditEntry[]>> {
  if (!isTauri()) return { data: [], live: false };
  try {
    const data = await invoke<{ entries: AdminApiAuditEntry[] }>("admin_api_get_audit", { limit });
    return { data: data.entries, live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}

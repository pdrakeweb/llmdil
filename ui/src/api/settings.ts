import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";

/** One filesystem rule of an MXC policy: access is read | write |
 *  readwrite | deny (manager-mxc's FsAccess, lowercase). */
export interface MxcFsRule { path: string; access: string }
/** One network rule: decision is allow | prompt | deny. */
export interface MxcNetRule { host: string; ports: number[]; decision: string }
export interface MxcCredentialRef { name: string; target: string }

/** The MXC sandbox policy document (manager-mxc's Policy). */
export interface MxcPolicy {
  version: number;
  filesystem: MxcFsRule[];
  network: MxcNetRule[];
  credentials: MxcCredentialRef[];
  default_deny: boolean;
  ui_access: boolean;
}

/** get_mxc_policy payload: the policy, where it came from ("file" when
 *  a policy file was read, "default" when the built-in default policy
 *  is in effect because no file exists), and the current mode. */
export interface MxcPolicyData {
  policy: MxcPolicy | null;
  source: string;
  mode: string;
}

/** get_mxc_policy — the active MXC sandbox policy with its provenance. */
export async function getMxcPolicy(): Promise<CallResult<MxcPolicyData | null>> {
  if (!isTauri()) return { data: null, live: false };
  try {
    const raw = await invoke<{ policy?: MxcPolicy; source?: string; mode?: string }>("get_mxc_policy");
    return {
      data: {
        policy: raw.policy ?? null,
        source: typeof raw.source === "string" ? raw.source : "",
        mode: typeof raw.mode === "string" ? raw.mode : "",
      },
      live: true,
    };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/** One observed decision in a Learning-mode activity report. `kind`
 *  serializes as the AccessKind variant name (FileRead | FileWrite |
 *  Network) when recorded. */
export interface MxcActivityEntry {
  timestamp: string;
  /** allow | prompt | deny */
  decision: string;
  description: string;
  kind: string | null;
  path_or_host: string | null;
  port: number | null;
  count: number;
}

export interface MxcActivityData {
  entries: MxcActivityEntry[];
  /** The report file path, or "none" when no report exists yet. */
  source: string;
}

/** mxc_get_activity_report — recent MXC sandbox decisions (Learning
 *  mode observations). An absent report is an empty entry list with
 *  source "none", not an error. */
export async function getMxcActivity(): Promise<CallResult<MxcActivityData | null>> {
  if (!isTauri()) return { data: null, live: false };
  try {
    const raw = await invoke<{ entries?: Partial<MxcActivityEntry>[]; source?: string }>("mxc_get_activity_report");
    const entries: MxcActivityEntry[] = (Array.isArray(raw.entries) ? raw.entries : []).map((e) => ({
      timestamp: typeof e.timestamp === "string" ? e.timestamp : "",
      decision: typeof e.decision === "string" ? e.decision : "",
      description: typeof e.description === "string" ? e.description : "",
      kind: typeof e.kind === "string" ? e.kind : null,
      path_or_host: typeof e.path_or_host === "string" ? e.path_or_host : null,
      port: typeof e.port === "number" ? e.port : null,
      count: typeof e.count === "number" ? e.count : 1,
    }));
    return { data: { entries, source: typeof raw.source === "string" ? raw.source : "" }, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/* ---------------- Telemetry actions ---------------- */

/** telemetry_set_interval — set the GPU poll interval (ms). Persists
 *  immediately, but the running poller's cadence is fixed at startup —
 *  the new interval applies on restart. */
export async function setTelemetryInterval(ms: number): Promise<CallResult<{ ok: boolean }>> {
  if (!isTauri()) return { data: { ok: true }, live: false };
  try {
    const data = await invoke<{ ok: boolean }>("telemetry_set_interval", { ms });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** telemetry_reset_peaks — reset recorded peak GPU values. */
export async function resetTelemetryPeaks(): Promise<CallResult<{ ok: boolean }>> {
  if (!isTauri()) return { data: { ok: true }, live: false };
  try {
    const data = await invoke<{ ok: boolean }>("telemetry_reset_peaks");
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** telemetry_save_snapshot — persist a telemetry snapshot to the data
 *  dir; returns the path it was written to. */
export async function saveTelemetrySnapshot(): Promise<CallResult<{ path: string }>> {
  if (!isTauri()) return { data: { path: "" }, live: false };
  try {
    const data = await invoke<{ path: string }>("telemetry_save_snapshot");
    return { data, live: true };
  } catch (e) {
    return { data: { path: "" }, live: true, error: errOf(e) };
  }
}

/** mxc_set_mode — set MXC enforcement mode: learning | enforced |
 *  disabled. `persisted` reports whether the change was saved to the
 *  configuration file; false means it applies to this session only.
 *  Sample mode returns an explicit not-available error (like the other
 *  action wrappers) — never a fake ok:true the view would announce as
 *  a mode change that did not happen. */
export async function setMxcMode(mode: string): Promise<CallResult<{ ok: boolean; mode: string; persisted?: boolean }>> {
  if (!isTauri()) return { data: { ok: false, mode }, live: false, error: "mxc_set_mode is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; mode: string; persisted?: boolean }>("mxc_set_mode", { mode });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false, mode }, live: true, error: errOf(e) };
  }
}

/** Notification preferences (Settings → Notifications). Mirrors
 *  `NotificationConfig` in manager-config: `enabled` is the master
 *  switch; each category switch is ANDed with it app-side. */
export interface NotificationSettings {
  enabled: boolean;
  operations: boolean;
  failures: boolean;
  activation: boolean;
}

export async function setNotificationConfig(config: NotificationSettings): Promise<CallResult<{ ok: boolean; persisted?: boolean }>> {
  if (!isTauri()) return { data: { ok: false }, live: false, error: "notifications_set_config is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; persisted?: boolean }>("notifications_set_config", { config });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** One component row of updates_check_all (manager-app `updates`). */
export interface ComponentUpdateReport {
  id: string;
  name: string;
  installed_version: string | null;
  latest_available: string | null;
  /** What Apply installs: the built-in verified pin (llama.cpp), or
   *  the newest acceptable release (LiteLLM — applied as a new pin). */
  target_version: string | null;
  pinned: boolean;
  pinned_version: string | null;
  auto_update: boolean;
  update_available: boolean;
  applyable: boolean;
  note: string | null;
  /** This component's release feed failed; the rest of the check stands. */
  check_error: string | null;
  source: string | null;
}

export interface ComponentUpdateCheck {
  checked_at: string;
  components: ComponentUpdateReport[];
}

/** updates_check_all — installed / latest / pinned per component.
 *  Per-component feed failures come back as check_error. */
export async function updatesCheckAll(): Promise<CallResult<ComponentUpdateCheck | null>> {
  if (!isTauri()) return { data: null, live: false, error: "updates_check_all is not available in sample mode" };
  try {
    const data = await invoke<ComponentUpdateCheck>("updates_check_all");
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/** updates_apply — start an update as a cancellable task (task_id is
 *  null when there was nothing to do). LiteLLM needs `version`: it
 *  moves only by a newer explicit pin. componentId "all" applies every
 *  applicable unpinned component. */
export async function updatesApply(componentId: string, version: string | null): Promise<CallResult<{ task_id: string | null; message?: string } | null>> {
  if (!isTauri()) return { data: null, live: false, error: "updates_apply is not available in sample mode" };
  try {
    const data = await invoke<{ task_id: string | null; message?: string }>("updates_apply", { componentId, version });
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/** One tracked long-running operation (tasks_list). */
export interface TaskRecordWire {
  id: string;
  kind: string;
  status: "running" | "done" | "failed" | "aborted";
  detail: unknown;
  created_ms: number;
}

/** tasks_list — every tracked task. */
export async function tasksList(): Promise<CallResult<TaskRecordWire[]>> {
  if (!isTauri()) return { data: [], live: false };
  try {
    const data = await invoke<TaskRecordWire[]>("tasks_list");
    return { data, live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}

/** task_abort — cancel a running task (bounded wait for it to stop). */
export async function taskAbort(taskId: string): Promise<CallResult<unknown>> {
  if (!isTauri()) return { data: null, live: false, error: "task_abort is not available in sample mode" };
  try {
    const data = await invoke<unknown>("task_abort", { taskId });
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/** updates_pin — pin a component to a version (or clear the pin with
 *  null) and persist it. */
export async function updatesPin(componentId: string, version: string | null): Promise<CallResult<{ ok: boolean; persisted?: boolean }>> {
  if (!isTauri()) return { data: { ok: false }, live: false, error: "updates_pin is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; persisted?: boolean }>("updates_pin", { componentId, version });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** updates_set_auto — set a component's auto-update flag and persist it. */
export async function updatesSetAuto(componentId: string, enabled: boolean): Promise<CallResult<{ ok: boolean; persisted?: boolean }>> {
  if (!isTauri()) return { data: { ok: false }, live: false, error: "updates_set_auto is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; persisted?: boolean }>("updates_set_auto", { componentId, enabled });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** App self-update state (app_update_status payload). Distinct
 *  from the per-component pins in the config's `updates` section:
 *  this is LLMdil's own GitHub-Releases check / stage / apply. */
export interface AppUpdateInfo {
  current_version: string;
  check_enabled: boolean;
  auto_update: boolean;
  last_checked_unix_ms: number | null;
  /** "up_to_date" | "update_available" | "failed" | null (never checked). */
  last_check_outcome: string | null;
  last_check_error: string | null;
  available: {
    version: string;
    tag: string;
    asset_name: string;
    size_bytes: number;
    sha256: string | null;
  } | null;
  staged: {
    version: string;
    asset_name: string;
    sha256: string;
    staged_at_unix_ms: number;
  } | null;
  last_stage_error: string | null;
  last_apply: {
    version: string;
    ok: boolean;
    message: string;
    at_unix_ms: number;
  } | null;
  /** State-derived: present exactly when a staged update newer
   *  than the running version exists. */
  banner: {
    version: string;
    staged_at_unix_ms: number;
    last_apply_error?: string;
  } | null;
}

const SAMPLE_APP_UPDATE: AppUpdateInfo = {
  current_version: "0.1.0-sample",
  check_enabled: true,
  auto_update: false,
  last_checked_unix_ms: null,
  last_check_outcome: null,
  last_check_error: null,
  available: null,
  staged: null,
  last_stage_error: null,
  last_apply: null,
  banner: null,
};

export async function getAppUpdateStatus(): Promise<CallResult<AppUpdateInfo>> {
  if (!isTauri()) return { data: SAMPLE_APP_UPDATE, live: false };
  try {
    const data = await invoke<AppUpdateInfo>("app_update_status");
    return { data, live: true };
  } catch (e) {
    return { data: SAMPLE_APP_UPDATE, live: true, error: errOf(e) };
  }
}

/** Run a check now. A failed check is an outcome in the returned
 *  state (last_check_outcome "failed"), not a call error. */
export async function appUpdateCheck(): Promise<CallResult<AppUpdateInfo>> {
  if (!isTauri()) return { data: SAMPLE_APP_UPDATE, live: false, error: "app_update_check is not available in sample mode" };
  try {
    const data = await invoke<AppUpdateInfo>("app_update_check");
    return { data, live: true };
  } catch (e) {
    return { data: SAMPLE_APP_UPDATE, live: true, error: errOf(e) };
  }
}

export async function appUpdateSetChecking(enabled: boolean): Promise<CallResult<AppUpdateInfo>> {
  if (!isTauri()) return { data: SAMPLE_APP_UPDATE, live: false, error: "app_update_set_checking is not available in sample mode" };
  try {
    const data = await invoke<AppUpdateInfo>("app_update_set_checking", { enabled });
    return { data, live: true };
  } catch (e) {
    return { data: SAMPLE_APP_UPDATE, live: true, error: errOf(e) };
  }
}

export async function appUpdateSetAuto(enabled: boolean): Promise<CallResult<AppUpdateInfo>> {
  if (!isTauri()) return { data: SAMPLE_APP_UPDATE, live: false, error: "app_update_set_auto is not available in sample mode" };
  try {
    const data = await invoke<AppUpdateInfo>("app_update_set_auto", { enabled });
    return { data, live: true };
  } catch (e) {
    return { data: SAMPLE_APP_UPDATE, live: true, error: errOf(e) };
  }
}

/** Download + verify + stage the available update (the running
 *  installation is not touched; applying needs the banner's
 *  Restart). */
export async function appUpdateDownload(): Promise<CallResult<AppUpdateInfo>> {
  if (!isTauri()) return { data: SAMPLE_APP_UPDATE, live: false, error: "app_update_download is not available in sample mode" };
  try {
    const data = await invoke<AppUpdateInfo>("app_update_download");
    return { data, live: true };
  } catch (e) {
    return { data: SAMPLE_APP_UPDATE, live: true, error: errOf(e) };
  }
}

/** Restart into the staged update: the app records the pending
 *  apply, hands off to the detached helper, and exits. */
export async function appUpdateRestart(): Promise<CallResult<{ ok: boolean; restarting?: boolean; version?: string }>> {
  if (!isTauri()) return { data: { ok: false }, live: false, error: "app_update_restart is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; restarting?: boolean; version?: string }>("app_update_restart");
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** Fire one toast immediately, bypassing the category switches —
 *  the Settings card's delivery check. */
export async function sendTestNotification(): Promise<CallResult<{ ok: boolean }>> {
  if (!isTauri()) return { data: { ok: false }, live: false, error: "notifications_send_test is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean }>("notifications_send_test");
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** One MXC self-test probe outcome (manager-mxc's SelfTestCase minus
 *  the request detail, which the view does not render). */
export interface MxcSelfTestCase {
  name: string;
  description: string;
  /** benign | malicious */
  kind: string;
  expected: string;
  actual: string;
  passed: boolean;
}

export interface MxcSelfTestData {
  cases: MxcSelfTestCase[];
  summary: { total: number; passed: number; failed: number };
}

/** mxc_run_self_test — evaluate the fixed probe set against the
 *  current policy. Pure policy evaluation: nothing is executed and no
 *  network traffic is sent. */
export async function mxcRunSelfTest(): Promise<CallResult<MxcSelfTestData>> {
  const empty: MxcSelfTestData = { cases: [], summary: { total: 0, passed: 0, failed: 0 } };
  if (!isTauri()) return { data: empty, live: false, error: "mxc_run_self_test is not available in sample mode" };
  try {
    const data = await invoke<MxcSelfTestData>("mxc_run_self_test");
    return { data, live: true };
  } catch (e) {
    return { data: empty, live: true, error: errOf(e) };
  }
}

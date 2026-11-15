import { isBusyError } from "../lifecycle-state";
import type { TransitionWire } from "../lifecycle-state";
import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";

/* ---------------- Settings ---------------- */

/** get_config — the full AppConfig JSON (data_dir, telemetry, mxc, updates, ...). */
/* ---------------- Windows service supervision ---------------- */

/** One managed service's row in the services_list payload. */
export interface ServiceRow {
  /** A backend id, "gateway", or "log-receiver". */
  target: string;
  /** backend | gateway | log-receiver (absent on older payloads). */
  kind?: string;
  service_name: string;
  registered: boolean;
  /** Raw SCM state (snake_case, or `{unknown: n}` for an unmodeled
   *  state), null when not registered. Render `lifecycle`, not this. */
  state: string | { unknown: number } | null;
  /** The shared lifecycle state (SCM state with exit codes: a stop
   *  with a non-zero code reads failed). Absent on older payloads —
   *  normalize with fromScm(state) then. */
  lifecycle?: string;
  /** The lifecycle operation in flight, or null. */
  transition?: TransitionWire | null;
  pid: number | null;
  /** The supervised child's pid (the shim is `pid`). */
  child_pid?: number | null;
  /** The service-specific exit code of the last stop, when non-zero. */
  exit_code?: number | null;
  /** Why the last exit was a failure (1 = credential/env resolution,
   *  2 = spawn failed, otherwise the child's exit code), or null. */
  failure_reason?: string | null;
}

/** services_list payload: mode, resolved account, shim presence, rows. */
export interface ServicesInfo {
  mode: string;
  account: string | null;
  host_path: string;
  host_present: boolean;
  services: ServiceRow[];
}

export async function servicesList(): Promise<CallResult<ServicesInfo>> {
  if (!isTauri()) {
    return { data: { mode: "process", account: null, host_path: "", host_present: false, services: [] }, live: false };
  }
  try {
    const data = await invoke<ServicesInfo>("services_list");
    return { data, live: true };
  } catch (e) {
    return { data: { mode: "process", account: null, host_path: "", host_present: false, services: [] }, live: true, error: errOf(e) };
  }
}

/** Persist the supervision mode through the generic config patch. */
export async function setSupervisionMode(mode: string): Promise<CallResult<{ ok: boolean; persisted?: boolean }>> {
  if (!isTauri()) return { data: { ok: false }, live: false, error: "update_config is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; persisted?: boolean }>("update_config", { patch: { supervision: { mode } } });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** update_config — merge a JSON patch into the stored configuration.
 *  The app validates the merged config and saves it atomically; the
 *  in-memory config changes only when the save succeeds, so a
 *  rejected patch leaves every setting untouched. The generic save
 *  path behind the Settings editors that have no dedicated op
 *  (WinML, PostgreSQL port, Admin API port, MXC policy path). */
export async function updateAppConfig(patch: Record<string, unknown>): Promise<CallResult<{ ok: boolean; persisted?: boolean }>> {
  if (!isTauri()) return { data: { ok: false }, live: false, error: "update_config is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; persisted?: boolean }>("update_config", { patch });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}

/** winml_probe result (manager-winml WinMlProbeReport). */
export interface WinMlProbeReport {
  base_url: string;
  models: string[];
  adapter_hint: string | null;
  limits: { supports_tensor_split?: boolean } & Record<string, unknown>;
}

/** "Re-check capabilities" on the WinML card: winml_probe re-probes
 *  WinMLServer. With a model registered it goes through the registry
 *  and refreshes the registration's limits (tech spec §6.5);
 *  otherwise it probes the configured port. */
export async function winmlRecheckCapabilities(): Promise<CallResult<WinMlProbeReport | null>> {
  if (!isTauri()) return { data: null, live: false, error: "winml_probe is not available in sample mode" };
  try {
    const data = await invoke<WinMlProbeReport>("winml_probe");
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

/** Register one service (backend id or "gateway") via the elevated
 *  helper. The helper prompts for the account password with the
 *  Windows system credential dialog; the app never sees it. */
export async function servicesRegister(target: string): Promise<CallResult<{ ok: boolean; service_name: string; account: string }>> {
  if (!isTauri()) return { data: { ok: false, service_name: "", account: "" }, live: false, error: "services_register is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; service_name: string; account: string }>("services_register", { target });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false, service_name: "", account: "" }, live: true, error: errOf(e) };
  }
}

export async function servicesUnregister(target: string): Promise<CallResult<{ ok: boolean; service_name: string }>> {
  if (!isTauri()) return { data: { ok: false, service_name: "" }, live: false, error: "services_unregister is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; service_name: string }>("services_unregister", { target });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false, service_name: "" }, live: true, error: errOf(e) };
  }
}

/* ---------------- Log receiver (Prometheus-local) ---------------- */

/** log_receiver_status payload: deployment state of the bundled
 *  log receiver service and the shipper's endpoint. */
export interface LogReceiverInfo {
  service_name: string;
  registered: boolean;
  /** Raw SCM state, null when not registered. Render `lifecycle`. */
  state: string | { unknown: number } | null;
  /** The shared lifecycle state: SCM state with exit codes, and
   *  running downgraded to degraded when the ingest port refuses
   *  connections. Absent on older payloads (fromScm(state) then). */
  lifecycle?: string;
  /** The receiver operation in flight, or null. */
  transition?: TransitionWire | null;
  /** The service-specific exit code of the last stop, when non-zero. */
  exit_code?: number | null;
  /** Why the last exit was a failure, or null. */
  failure_reason?: string | null;
  /** The ingest port and whether it accepted a TCP connect. */
  ingest_port?: number;
  ingest_ok?: boolean;
  pid: number | null;
  receiver_exe: string;
  receiver_present: boolean;
  data_dir: string;
  ingest_token_seeded: boolean;
  read_token_seeded: boolean;
  endpoint: string;
  endpoint_is_local: boolean;
  streaming_enabled: boolean;
  /** What the log shipper is doing, per source. Absent from
   *  payloads produced before the per-source status existed. */
  shipper?: LogShipperInfo;
}

/** One log-streaming source's row: `active` (tailed or subscribed,
 *  input exists), `disabled` (toggled off — not shipping),
 *  `file_missing` (enabled, its log file does not exist),
 *  `waiting` (a backend with nothing captured yet), `local_only`
 *  (on, but held: ships only to a loopback endpoint), `unavailable`
 *  (enabled, no in-process feed), `not_running` (enabled in the
 *  config, but the shipper itself is not running). */
export interface LogShipperSource {
  source: string;
  state: string;
  /** Records queued from this source since the shipper started. */
  records: number;
}

/** The `shipper` object of the log_receiver_status payload. The
 *  counters are present only while the shipper runs. */
export interface LogShipperInfo {
  running: boolean;
  /** starting | connected | spooling | not_running */
  state: string;
  /** A saved setting still differs from what the running shipper
   *  started with. Saved settings apply as they are saved, so this
   *  is normally false. */
  restart_pending: boolean;
  /** Why streaming is on but no shipper is running (refused
   *  endpoint, missing token, …); null otherwise. */
  note?: string | null;
  /** The saved switches, for the card's editor: the master switch
   *  and each key of `log_streaming.sources`. */
  configured?: { enabled: boolean; sources: Record<string, boolean> };
  queued?: number;
  spooled_bytes?: number;
  delivered?: number;
  dropped_queue?: number;
  dropped_spool?: number;
  dropped_events?: number;
  last_delivered?: string | null;
  sources: LogShipperSource[];
}

const LR_SAMPLE: LogReceiverInfo = {
  service_name: "llm-log-receiver", registered: false, state: null, pid: null,
  receiver_exe: "", receiver_present: false, data_dir: "",
  ingest_token_seeded: false, read_token_seeded: false,
  endpoint: "", endpoint_is_local: false, streaming_enabled: false,
};

export async function logReceiverStatus(): Promise<CallResult<LogReceiverInfo>> {
  if (!isTauri()) return { data: LR_SAMPLE, live: false };
  try {
    const data = await invoke<LogReceiverInfo>("log_receiver_status");
    return { data, live: true };
  } catch (e) {
    return { data: LR_SAMPLE, live: true, error: errOf(e) };
  }
}

/** Deploy or repair the log receiver service (one UAC prompt).
 *  `read_token_new` is set exactly once — on the deploy that
 *  generated the read token — for Secure Vault filing. */
export async function logReceiverDeploy(): Promise<CallResult<{ ok: boolean; service_name: string; read_token_new: string | null }>> {
  if (!isTauri()) return { data: { ok: false, service_name: "", read_token_new: null }, live: false, error: "log_receiver_deploy is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; service_name: string; read_token_new: string | null }>("log_receiver_deploy");
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false, service_name: "", read_token_new: null }, live: true, error: errOf(e) };
  }
}

/** Remove the log receiver service + its firewall rule (one UAC
 *  prompt). The store and token files stay in the data directory. */
export async function logReceiverRemove(): Promise<CallResult<{ ok: boolean; service_name: string }>> {
  if (!isTauri()) return { data: { ok: false, service_name: "" }, live: false, error: "log_receiver_remove is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; service_name: string }>("log_receiver_remove");
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false, service_name: "" }, live: true, error: errOf(e) };
  }
}

/** Flip the shipper endpoint to the local receiver. Backs up
 *  config.json first (path in `backup`); applies on app restart. */
export async function logReceiverFlipEndpoint(): Promise<CallResult<{ ok: boolean; endpoint: string; already: boolean; backup: string | null }>> {
  if (!isTauri()) return { data: { ok: false, endpoint: "", already: false, backup: null }, live: false, error: "log_receiver_flip_endpoint is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; endpoint: string; already: boolean; backup: string | null }>("log_receiver_flip_endpoint");
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false, endpoint: "", already: false, backup: null }, live: true, error: errOf(e) };
  }
}

/** The log_receiver_start/stop/restart result. */
export interface LogReceiverOpResult {
  ok: boolean;
  service_name: string;
  /** start | stop | restart */
  action: string;
  detail?: string | null;
  /** The ingest port accepted a connection after the operation. */
  ingest_ok?: boolean;
  /** The receiver's lifecycle state after the operation. */
  lifecycle?: string;
  pid?: number | null;
}

async function logReceiverOp(
  cmd: "log_receiver_start" | "log_receiver_stop" | "log_receiver_restart",
  opts?: { origin?: string },
): Promise<CallResult<LogReceiverOpResult> & { busy?: boolean }> {
  const empty: LogReceiverOpResult = { ok: false, service_name: "", action: cmd.replace("log_receiver_", "") };
  if (!isTauri()) return { data: empty, live: false, error: `${cmd} is not available in sample mode` };
  try {
    const args: Record<string, unknown> = {};
    if (opts?.origin) args.origin = opts.origin;
    const data = await invoke<LogReceiverOpResult>(cmd, args);
    return { data, live: true };
  } catch (e) {
    const error = errOf(e);
    return { data: empty, live: true, error, busy: isBusyError(error) };
  }
}

/** Start / stop / restart the log receiver service (unelevated; the
 *  service DACL grants start/stop). Markers go to the receiver's own
 *  lifecycle log. */
export function logReceiverStart(opts?: { origin?: string }) { return logReceiverOp("log_receiver_start", opts); }
export function logReceiverStop(opts?: { origin?: string }) { return logReceiverOp("log_receiver_stop", opts); }
export function logReceiverRestart(opts?: { origin?: string }) { return logReceiverOp("log_receiver_restart", opts); }

export async function getAppConfig(): Promise<CallResult<Record<string, unknown> | null>> {
  if (!isTauri()) return { data: null, live: false };
  try {
    const data = await invoke<Record<string, unknown>>("get_config");
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

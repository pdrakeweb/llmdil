import { isBusyError } from "../lifecycle-state";
import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";

/** One saved change inside a pending activation plan. */
export interface ActivationChange {
  ts: string;
  kind: string;
  summary: string;
  /** What activates the change: "gateway" (a gateway restart) or
   *  "backend" (the target backends' next (re)start). */
  scope?: string;
  /** Backend ids a backend-scope change applies to. */
  targets?: string[];
  /** How the change goes live: "gateway_restart" or "restart"
   *  (a running process must restart), or "next_start" (nothing
   *  running holds the old state). */
  activation?: string;
}

/** gateway_activation_plan — every saved configuration change that
 *  is not live yet, across scopes: gateway changes wait on a
 *  gateway restart, backend changes (flags, tensor split, model
 *  assignment) wait on their backend's (re)start. The plan is inert
 *  until the operator applies it, defers it, or cancels the
 *  changes. */
export interface ActivationPlanData {
  pending: boolean;
  gateway_up: boolean;
  /** A gateway-side change is pending (journal entry or drift). */
  gateway_pending?: boolean;
  /** Live drift against the RUNNING gateway: the files it was
   *  started with differ from the saved configuration. Always
   *  false while the gateway is down — a stale file then is not
   *  pending work, since the next start regenerates it. */
  drift: boolean;
  /** The raw generated-vs-on-disk file comparison, in any
   *  gateway state. Informational while the gateway is down (the
   *  next start regenerates the files); when drift is pending
   *  work it is also itemized in `changes` as a `gateway_drift`
   *  item, so a pending plan never counts zero changes. */
  files_differ?: boolean;
  /** Pending while the gateway is down: the changes take effect at
   *  the next start rather than needing a restart. */
  applies_at_next_start: boolean;
  /** Something Apply can do right now: restart the gateway (or
   *  start it) or restart a running backend. */
  actionable?: boolean;
  /** Running backends a plan application restarts. */
  backend_restart_ids?: string[];
  changes: ActivationChange[];
  /** A pre-change config snapshot exists, so cancel can restore it. */
  snapshot_available: boolean;
}

const EMPTY_PLAN: ActivationPlanData = {
  pending: false, gateway_up: false, drift: false, files_differ: false,
  applies_at_next_start: false, changes: [], snapshot_available: false,
};

export async function getActivationPlan(): Promise<CallResult<ActivationPlanData>> {
  if (!isTauri()) return { data: EMPTY_PLAN, live: false };
  try {
    const data = await invoke<ActivationPlanData>("gateway_activation_plan");
    return { data, live: true };
  } catch (e) {
    return { data: EMPTY_PLAN, live: true, error: errOf(e) };
  }
}

/** gateway_activation_cancel — restore the pre-change snapshot and
 *  clear the plan. Refused (error) when the config has since changed
 *  through another path. */
export async function activationCancelChanges(): Promise<CallResult<{ cancelled: boolean; reason?: string }>> {
  if (!isTauri()) return { data: { cancelled: false }, live: false };
  try {
    const data = await invoke<{ cancelled: boolean; reason?: string }>("gateway_activation_cancel");
    return { data, live: true };
  } catch (e) {
    return { data: { cancelled: false }, live: true, error: errOf(e) };
  }
}

/** gateway_activation_complete — clear the plan after a successful
 *  gateway (re)start made the saved changes live. */
export async function activationComplete(): Promise<CallResult<{ cleared: boolean }>> {
  if (!isTauri()) return { data: { cleared: false }, live: false };
  try {
    const data = await invoke<{ cleared: boolean }>("gateway_activation_complete");
    return { data, live: true };
  } catch (e) {
    return { data: { cleared: false }, live: true, error: errOf(e) };
  }
}

/** The activation_apply result (the one server-side apply: backend
 *  restarts, then the gateway restart, each under its lifecycle
 *  transition; history recorded once). */
export interface ActivationApplyResult {
  ok: boolean;
  /** nothing_pending | completed | completed_with_failures | failed */
  status: string;
  message: string;
  mode?: string;
  source?: string;
  /** Present when a failed gateway restart produced manual commands. */
  manual_command?: string | null;
  restarted_backends: string[];
  gateway_restarted: boolean;
  quiesce?: { drained: boolean; in_flight: number; timed_out: boolean } | null;
  execution_id?: string | null;
  items: ActivationExecutionItem[];
  /** The server refused: another apply is running (`error` names it). */
  busy?: boolean;
}

/** activation_apply — apply the pending activation plan server-side.
 *  `mode` "drain" drains in-flight gateway requests before the gateway
 *  restart; "now" restarts at once. The plan card's spellings are
 *  accepted too ("quiesce" = drain, "restart"/"start" = now) and
 *  recorded as given, so the history keeps its mode labels. `source`
 *  is the surface recorded in the history ("desktop" for the app).
 *  Progress is visible meanwhile as gateway_status.activation_applying. */
export async function activationApply(mode: "drain" | "now" | "quiesce" | "restart" | "start", source = "desktop"):Promise<CallResult<ActivationApplyResult>> {
  const empty: ActivationApplyResult = { ok: false, status: "failed", message: "", restarted_backends: [], gateway_restarted: false, items: [] };
  if (!isTauri()) return { data: { ...empty, message: "activation_apply is not available in sample mode" }, live: false, error: "activation_apply is not available in sample mode" };
  try {
    const raw = await invoke<Partial<ActivationApplyResult>>("activation_apply", { mode, source });
    return {
      data: {
        ...empty,
        ...raw,
        ok: raw.ok === true,
        status: typeof raw.status === "string" ? raw.status : raw.ok ? "completed" : "failed",
        message: typeof raw.message === "string" ? raw.message : "",
        restarted_backends: Array.isArray(raw.restarted_backends) ? raw.restarted_backends : [],
        gateway_restarted: raw.gateway_restarted === true,
        items: Array.isArray(raw.items) ? raw.items : [],
      },
      live: true,
    };
  } catch (e) {
    const error = errOf(e);
    return { data: { ...empty, message: error, busy: isBusyError(error) || undefined }, live: true, error };
  }
}

/** One plan change inside a recorded activation execution, with
 *  the outcome the apply recorded for it. */
export interface ActivationExecutionItem {
  /** When the change was saved (the journal entry's timestamp). */
  ts: string;
  kind: string;
  summary: string;
  scope: string;
  targets: string[];
  activation: string;
  /** "applied", "skipped", or "failed". */
  outcome: string;
  /** Skip reason or failure error text; absent for applied items. */
  detail?: string;
}

/** One recorded activation plan execution (a whole apply). Status:
 *  "completed", "completed_with_failures", or "failed" (aborted
 *  before every step was attempted). */
export interface ActivationExecution {
  id: string;
  started_at: string;
  finished_at: string;
  status: string;
  /** The surface that initiated the apply ("desktop"). */
  source: string;
  /** Apply mode: "quiesce", "restart", or "start". */
  mode?: string;
  drift: boolean;
  gateway_up: boolean;
  gateway_pending: boolean;
  items: ActivationExecutionItem[];
}

/** The list form of a recorded execution: identity, times, status,
 *  and per-outcome item counts (the full items ride the detail). */
export interface ActivationExecutionSummary {
  id: string;
  started_at: string;
  finished_at: string;
  status: string;
  source: string;
  mode?: string;
  drift: boolean;
  gateway_up: boolean;
  gateway_pending: boolean;
  items: number;
  applied: number;
  skipped: number;
  failed: number;
}

/** gateway_activation_record_execution — report one finished
 *  activation plan apply for the durable execution history. The
 *  apply funnel calls this when an apply ends (including an aborted
 *  one) and ignores the result: recording is best-effort and must
 *  never affect the activation it describes. The record argument is
 *  the execution without its server-assigned id. */
export async function recordActivationExecution(
  record: Omit<ActivationExecution, "id">,
): Promise<CallResult<{ recorded: boolean; id?: string | null }>> {
  if (!isTauri()) return { data: { recorded: false }, live: false };
  try {
    const data = await invoke<{ recorded: boolean; id?: string | null }>("gateway_activation_record_execution", { record });
    return { data, live: true };
  } catch (e) {
    return { data: { recorded: false }, live: true, error: errOf(e) };
  }
}

/** gateway_activation_history — the recorded activation plan
 *  executions, newest first. With an execution id, that execution's
 *  full record (or null when it has aged out of the retained
 *  history). */
export async function getActivationHistory(): Promise<CallResult<{ executions: ActivationExecutionSummary[] }>> {
  if (!isTauri()) return { data: { executions: [] }, live: false };
  try {
    const data = await invoke<{ executions: ActivationExecutionSummary[] }>("gateway_activation_history");
    return { data, live: true };
  } catch (e) {
    return { data: { executions: [] }, live: true, error: errOf(e) };
  }
}

export async function getActivationExecution(id: string): Promise<CallResult<{ execution: ActivationExecution | null }>> {
  if (!isTauri()) return { data: { execution: null }, live: false };
  try {
    const data = await invoke<{ execution: ActivationExecution | null }>("gateway_activation_history", { executionId: id });
    return { data, live: true };
  } catch (e) {
    return { data: { execution: null }, live: true, error: errOf(e) };
  }
}

/** gateway_quiesce_wait — wait for in-flight gateway requests to
 *  drain to zero (bounded by timeoutSecs); reports the final count
 *  when the wait times out. */
export async function quiesceWait(timeoutSecs: number): Promise<CallResult<{ drained: boolean; in_flight: number; timed_out: boolean }>> {
  if (!isTauri()) return { data: { drained: true, in_flight: 0, timed_out: false }, live: false };
  try {
    const data = await invoke<{ drained: boolean; in_flight: number; timed_out: boolean }>("gateway_quiesce_wait", { timeoutSecs });
    return { data, live: true };
  } catch (e) {
    return { data: { drained: false, in_flight: 0, timed_out: false }, live: true, error: errOf(e) };
  }
}

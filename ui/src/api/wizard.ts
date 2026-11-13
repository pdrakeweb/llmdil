import { trim1 } from "../charts";
import { mockWizardSteps } from "../mock";
import type { WizardStepState } from "../types";
import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";

export interface WizardStepOutcome {
  state: string;
  message: string;
  log_tail: string[];
  duration_ms: number;
  /** Ready-to-run manual commands attached to a failure (the
   * start-gateway step sets this): rendered as a code block under
   * the failure message. */
  manual_command?: string | null;
}

/** The run_wizard_step result. `busy` is true when another step
 * already holds the wizard lock: a running state to report, not a
 * failure, with `outcome` null. When the backend knows which step
 * is in flight, `running_step`/`elapsed_secs` say what and for how
 * long. */
export interface WizardStepRun {
  task_id: string;
  /** The step succeeded (done or skipped). False for a failed or
   *  manual outcome and for a busy refusal. Derived from the outcome
   *  on payloads from before the server sent `ok`. */
  ok: boolean;
  busy: boolean;
  running_step?: string;
  elapsed_secs?: number;
  outcome: WizardStepOutcome | null;
}

/** Human elapsed for a busy report: "45s", "12 min", "1 h 5 min". */
function fmtElapsedSecs(secs: number): string {
  if (secs < 60) return `${secs}s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins} min`;
  const rem = mins % 60;
  return rem ? `${Math.floor(mins / 60)} h ${rem} min` : `${Math.floor(mins / 60)} h`;
}

/** The message for a busy run_wizard_step result: names the
 * in-flight step and its elapsed time when the backend reported
 * them, else the caller's fallback text. */
export function wizardBusyMessage(data: { running_step?: string; elapsed_secs?: number }, fallback: string): string {
  if (data.running_step) {
    const elapsed = typeof data.elapsed_secs === "number" ? ` for ${fmtElapsedSecs(data.elapsed_secs)}` : "";
    return `${data.running_step} has been running${elapsed} — a setup step is still running; wait for it to finish, then try again.`;
  }
  return fallback;
}

/** run_wizard_step — runs one step to completion; the outcome is in the
 * result. `busy` is true when another step already holds the wizard
 * lock: that is a running state to report, not a failure, and
 * `outcome` is null. */
export async function runWizardStep(stepId: string, opts?: { origin?: string }): Promise<CallResult<WizardStepRun>> {
  if (isTauri()) {
    try {
      const args: Record<string, unknown> = { stepId };
      if (opts?.origin) args.origin = opts.origin;
      const data = await invoke<{ task_id: string; ok?: boolean; busy?: boolean; running_step?: string; elapsed_secs?: number; outcome: WizardStepOutcome | null }>("run_wizard_step", args);
      const busy = Boolean(data.busy);
      const outcome = data.outcome ?? null;
      const ok = typeof data.ok === "boolean"
        ? data.ok
        : !busy && outcome !== null && (outcome.state === "done" || outcome.state === "skipped");
      return { data: { task_id: data.task_id, ok, busy, running_step: data.running_step, elapsed_secs: data.elapsed_secs, outcome }, live: true };
    } catch (e) {
      return { data: { task_id: "", ok: false, busy: false, outcome: { state: "failed", message: String(e), log_tail: [], duration_ms: 0 } }, live: true, error: errOf(e) };
    }
  }
  return { data: { task_id: "sample-task", ok: true, busy: false, outcome: { state: "done", message: "sample mode", log_tail: [], duration_ms: 0 } }, live: false };
}

/** run_audit — re-run all check-type steps in audit mode. The response
 * carries the audit's per-step results: the checklist is otherwise
 * painted from the install wizard's state, which an audit never
 * changes, so the caller paints these steps to make the outcome
 * visible. */
export interface AuditResult {
  task_id: string;
  done: number;
  total: number;
  steps: WizardStepState[];
  /** True when an audit was already in flight (e.g. the launch-time
   * hydration audit): a running state to report, not a failure. The
   * running audit's results land in the checklist when it finishes. */
  already_running: boolean;
}

export async function runAudit(): Promise<CallResult<AuditResult>> {
  if (isTauri()) {
    try {
      const r = await invoke<{ task_id: string; done: number; total: number; steps: BackendStep[]; already_running?: boolean }>("run_audit");
      return { data: { task_id: r.task_id, done: r.done, total: r.total, steps: r.steps.map(mapBackendStep), already_running: Boolean(r.already_running) }, live: true };
    } catch (e) {
      return { data: { task_id: "", done: 0, total: 0, steps: [], already_running: false }, live: true, error: errOf(e) };
    }
  }
  return { data: { task_id: "sample-task", done: 0, total: 0, steps: [], already_running: false }, live: false };
}

/** One command a wizard step will run (setup_get_commands wire shape). */
export interface PlannedCommandWire {
  argv: string[];
  cwd: string | null;
  env: Record<string, string>;
  stdout_log: string | null;
  stderr_log: string | null;
}

/** setup_get_commands — the exact commands a step runs, for the
 * failed-step "run it manually" block in the wizard. */
export async function getWizardCommands(stepId: string): Promise<CallResult<PlannedCommandWire[]>> {
  if (!isTauri()) return { data: [], live: false };
  try {
    const data = await invoke<PlannedCommandWire[]>("setup_get_commands", { stepId });
    return { data: Array.isArray(data) ? data : [], live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}

interface BackendStep {
  id: string;
  name: string;
  state: WizardStepState["state"];
  message: string;
  duration_ms: number;
  log_tail: string[];
  note: string | null;
  manual_command: string | null;
  remediation: string | null;
  help_url: string | null;
}

/** setup_get_steps — live wizard step states under Tauri; sample data in the browser. */
/** Map one backend step record (setup_get_steps / run_audit shape)
 * into the UI's step state. */
function mapBackendStep(s: BackendStep): WizardStepState {
  return {
    id: s.id,
    name: s.name,
    detail: s.message,
    state: s.state,
    duration: s.duration_ms > 0 ? `${trim1(s.duration_ms / 1000)}s` : undefined,
    note: s.note ?? undefined,
    helpUrl: s.help_url ?? undefined,
    message: s.message,
    logTail: s.log_tail,
    manualCommand: s.manual_command ?? undefined,
    remediation: s.remediation ?? undefined,
  };
}

export async function getWizardSteps(): Promise<CallResult<WizardStepState[]>> {
  if (!isTauri()) return { data: mockWizardSteps(), live: false };
  try {
    const r = await invoke<{ steps: BackendStep[]; done: number; total: number }>("setup_get_steps");
    return { data: r.steps.map(mapBackendStep), live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}

/** setup_status — whether the setup wizard has been completed. */
export async function getSetupStatus(): Promise<CallResult<{ setup_completed: boolean }>> {
  if (!isTauri()) return { data: { setup_completed: true }, live: false };
  try {
    const data = await invoke<{ setup_completed: boolean }>("setup_status");
    return { data, live: true };
  } catch (e) { return { data: { setup_completed: true }, live: false, error: String(e) }; }
}

/** setup_mark_complete — mark the setup wizard complete (persisted).
 *  The command now rejects on save failure (a proper Err, like every
 *  other command); the rejection is normalized here into
 *  { ok: false, persisted: false } + error so callers keep reading the
 *  outcome from the result instead of an unhandled rejection. */
export async function markSetupComplete(): Promise<CallResult<{ ok: boolean; persisted: boolean }>> {
  if (!isTauri()) return { data: { ok: true, persisted: false }, live: false };
  try {
    const data = await invoke<{ ok: boolean; persisted: boolean }>("setup_mark_complete");
    return { data, live: true };
  } catch (e) { return { data: { ok: false, persisted: false }, live: true, error: errOf(e) }; }
}

/** setup_step_help_url — help URL a wizard step offers (e.g. driver download page). */
export async function getStepHelpUrl(stepId: string): Promise<CallResult<{ url: string | null; step_id: string }>> {
  if (!isTauri()) return { data: { url: null, step_id: stepId }, live: false };
  try {
    const data = await invoke<{ url: string | null; step_id: string }>("setup_step_help_url", { stepId });
    return { data, live: true };
  } catch (e) { return { data: { url: null, step_id: stepId }, live: false, error: String(e) }; }
}

/** open_url — open an external URL in the system browser (http/https only). */
export async function openUrl(url: string): Promise<CallResult<{ ok: boolean }>> {
  if (!isTauri()) { window.open(url, "_blank", "noopener"); return { data: { ok: true }, live: false }; }
  try {
    const data = await invoke<{ ok: boolean }>("open_url", { url });
    return { data, live: true };
  } catch (e) { return { data: { ok: false }, live: false, error: String(e) }; }
}

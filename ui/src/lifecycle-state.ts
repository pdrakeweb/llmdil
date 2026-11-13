/* Service lifecycle vocabulary — the pure half of the shared lifecycle
   module (ui/src/lifecycle.ts re-exports everything here; import from
   there). It is a separate file only so api.ts can use the normalizers
   and the verdict rule without an import cycle (lifecycle.ts calls the
   api wrappers), and so the rules can be checked without a DOM.

   One vocabulary for every managed service — backends, the gateway,
   PostgreSQL, the log receiver — in process and service (SCM) mode.
   The strings are the wire strings of manager_supervisor::lifecycle::
   LifecycleState (snake_case), so a status payload's `state` /
   `lifecycle` field is used as is. GPU "degraded" (telemetry health)
   is NOT lifecycle and stays on the separate Status type. */

import type { Status } from "./types";

export type LifecycleState =
  | "running"
  | "starting"
  | "restarting"
  | "stopping"
  | "stopped"
  | "failed"
  | "degraded"
  | "unknown"
  | "not_installed";

export const LIFECYCLE_STATES: readonly LifecycleState[] = [
  "running", "starting", "restarting", "stopping", "stopped", "failed", "degraded", "unknown", "not_installed",
];

export type TransitionKind = "starting" | "stopping" | "restarting";

/** An in-flight lifecycle operation as the status payloads send it. */
export interface TransitionWire {
  kind: TransitionKind;
  /** Who asked: ui, dashboard, activation, api, cli, wizard, bench,
   *  optimize, monitor, startup, scm. */
  origin: string;
  /** RFC 3339 instant the operation began. */
  since: string;
}

/** The last unexpected failure of a service (cleared at the next start). */
export interface FailureWire {
  at: string;
  reason: string;
  exit_code: number | null;
}

export type DotClass = "ok" | "warn" | "err" | "idle";
export type TagClass = "status-on" | "status-transition" | "status-bad" | "status-off";

export interface LifecycleMeta {
  label: string;
  dot: DotClass;
  tag: TagClass;
  color: string;
  /** The process is (or may be) serving: running, starting,
   *  restarting, degraded. Same rule as LifecycleState::is_live. */
  isLive: boolean;
  /** starting, restarting, stopping. */
  isTransition: boolean;
}

/** Palette shared with the dashboard/map SVG (CSS --ok/--warn/--err/--text3). */
export const LIFECYCLE_COLORS = { green: "#3fb950", amber: "#d29922", red: "#f85149", grey: "#6d7686" } as const;

const C = LIFECYCLE_COLORS;

export const META: Record<LifecycleState, LifecycleMeta> = {
  running: { label: "running", dot: "ok", tag: "status-on", color: C.green, isLive: true, isTransition: false },
  starting: { label: "starting…", dot: "warn", tag: "status-transition", color: C.amber, isLive: true, isTransition: true },
  restarting: { label: "restarting…", dot: "warn", tag: "status-transition", color: C.amber, isLive: true, isTransition: true },
  stopping: { label: "stopping…", dot: "warn", tag: "status-transition", color: C.amber, isLive: false, isTransition: true },
  stopped: { label: "stopped", dot: "idle", tag: "status-off", color: C.grey, isLive: false, isTransition: false },
  failed: { label: "failed", dot: "err", tag: "status-bad", color: C.red, isLive: false, isTransition: false },
  degraded: { label: "not responding", dot: "warn", tag: "status-transition", color: C.amber, isLive: true, isTransition: false },
  unknown: { label: "unknown", dot: "idle", tag: "status-off", color: C.grey, isLive: false, isTransition: false },
  not_installed: { label: "not registered", dot: "idle", tag: "status-off", color: C.grey, isLive: false, isTransition: false },
};

export function isLifecycleState(s: unknown): s is LifecycleState {
  return typeof s === "string" && (LIFECYCLE_STATES as readonly string[]).includes(s);
}

/** Map an SCM service state (manager_services::ServiceState serde form:
 *  snake_case, or `{unknown: n}` for a raw state the crate does not
 *  model) to the lifecycle vocabulary. A bare `stopped` carries no exit
 *  code here, so it reads stopped; payloads that know the exit code
 *  send `lifecycle` (stopped vs failed) and that wins. */
export function fromScm(s: unknown): LifecycleState {
  if (s && typeof s === "object" && "unknown" in (s as Record<string, unknown>)) return "unknown";
  switch (s) {
    case "running": return "running";
    case "start_pending": return "starting";
    case "stop_pending": return "stopping";
    case "continue_pending": case "pause_pending": return "starting";
    case "paused": return "degraded";
    case "stopped": return "stopped";
    case "not_installed": return "not_installed";
    default: return "unknown";
  }
}

/** Normalize any state string the UI receives — the unified lifecycle
 *  wire strings, the older supervisor process vocabulary, and the SCM
 *  snake_case strings (or `{unknown: n}`) — to a LifecycleState.
 *  Anything unrecognized (including null/undefined) is "unknown". */
export function fromWire(s: unknown): LifecycleState {
  if (isLifecycleState(s)) return s;
  if (typeof s === "string") {
    switch (s.toLowerCase().replace(/[\s-]+/g, "_")) {
      case "running": case "up": case "ok": return "running";
      case "starting": case "backoff": return "starting";
      case "restarting": return "restarting";
      case "stopping": return "stopping";
      case "stopped": case "idle": return "stopped";
      case "failed": case "down": case "crashed": return "failed";
      case "degraded": case "not_responding": return "degraded";
      case "not_installed": case "not_registered": return "not_installed";
      default: break;
    }
  }
  return fromScm(s);
}

/** The lifecycle state -> the dashboard Status vocabulary (cards, GPU
 *  placement rows). Lifecycle "degraded" (alive, not answering) maps
 *  to Status "degraded"; stopped/unknown/not registered are idle. */
export function toStatus(s: LifecycleState): Status {
  switch (s) {
    case "running": return "ok";
    case "starting": return "starting";
    case "restarting": return "restarting";
    case "stopping": return "stopping";
    case "failed": return "down";
    case "degraded": return "degraded";
    default: return "idle";
  }
}

/** True when an op error string is the server's Busy refusal (another
 *  lifecycle operation holds the service): "<service> <kind> already in
 *  progress (started by …)", or an activation apply already running. */
export function isBusyError(msg: string | null | undefined): boolean {
  if (!msg) return false;
  return /already in progress|activation apply started by .* is running|^busy\b/i.test(msg);
}

/** "12 s", "3 min", "1 h 5 min" since an RFC 3339 instant; "" when unparseable. */
export function elapsedSince(since: string | null | undefined, now: number = Date.now()): string {
  if (!since) return "";
  const t = Date.parse(since);
  if (!Number.isFinite(t)) return "";
  const secs = Math.max(0, Math.round((now - t) / 1000));
  if (secs < 60) return `${secs} s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins} min`;
  const rem = mins % 60;
  return rem ? `${Math.floor(mins / 60)} h ${rem} min` : `${Math.floor(mins / 60)} h`;
}

/** The state's label, with the transition's origin and age when one is
 *  in flight: "restarting… (activation, 12 s)". */
export function lifecycleLabel(s: LifecycleState, t?: TransitionWire | null, now: number = Date.now()): string {
  const base = META[s].label;
  if (!t) return base;
  const parts = [t.origin, elapsedSince(t.since, now)].filter(Boolean);
  return parts.length ? `${base} (${parts.join(", ")})` : base;
}

export function lifecycleColor(s: LifecycleState): string {
  return META[s].color;
}

/** Edge styling for the topology views: running = hot, transition or
 *  not responding = warn (amber dashed), failed = red inline stroke,
 *  everything else plain. */
export function lifecycleEdge(s: LifecycleState): { cls: string; stroke: string } {
  if (s === "running") return { cls: "hot", stroke: "" };
  if (META[s].isTransition || s === "degraded") return { cls: "warn", stroke: "" };
  if (s === "failed") return { cls: "", stroke: C.red };
  return { cls: "", stroke: "" };
}

/* ---------------- Actions (pure rules) ---------------- */

export type LifecycleKind = "backend" | "gateway" | "postgres" | "log-receiver";
export type LifecycleAction = "start" | "stop" | "restart";

export interface LifecycleActionCtx {
  kind: LifecycleKind;
  /** False for a configured-but-disabled backend: never a start target. */
  enabled?: boolean;
  /** False for a supervisor-only orphan row (id not in the config). */
  configured?: boolean;
  /** When set, every action is disabled with this reason (e.g. the
   *  database Stop while the gateway still runs). */
  blockedReason?: string;
}

export interface LifecycleActionSpec {
  action: LifecycleAction;
  label: string;
  disabled: boolean;
  reason?: string;
}

const ACTION_LABEL: Record<LifecycleAction, string> = { start: "Start", stop: "Stop", restart: "Restart" };

/** The one Start/Stop/Restart rule for every card:
 *  - running / degraded / starting: Stop + Restart (Restart disabled
 *    unless enabled and configured — the server refuses it otherwise);
 *  - restarting / stopping: the same buttons, disabled until it settles;
 *  - stopped / failed: Start (disabled unless enabled and configured)
 *    + Restart disabled (nothing to restart);
 *  - not registered / unknown: none — except the gateway, whose own
 *    page must stay operable when its status call fails: Start, Stop
 *    and Restart, enabled, titled "Gateway status unavailable".
 *  PostgreSQL offers Stop only — it starts with the gateway. */
export function lifecycleActions(s: LifecycleState, ctx: LifecycleActionCtx): LifecycleActionSpec[] {
  const mk = (action: LifecycleAction, disabled: boolean, reason?: string): LifecycleActionSpec =>
    ({ action, label: ACTION_LABEL[action], disabled, ...(reason ? { reason } : {}) });
  let notStartable: string | undefined;
  if (ctx.configured === false) notStartable = "Not in the configuration";
  else if (ctx.enabled === false) notStartable = "Disabled — enable it to start";
  let out: LifecycleActionSpec[];
  switch (s) {
    case "running": case "degraded": case "starting":
      out = [mk("stop", false), mk("restart", Boolean(notStartable), notStartable)];
      break;
    case "restarting": case "stopping": {
      const why = `${META[s].label.replace(/…$/, "")} — wait for it to finish`;
      out = [mk("stop", true, why), mk("restart", true, why)];
      break;
    }
    case "stopped": case "failed":
      out = [mk("start", Boolean(notStartable), notStartable), mk("restart", true, "Not running")];
      break;
    case "unknown":
      if (ctx.kind === "gateway") {
        const why = "Gateway status unavailable";
        out = [mk("start", false, why), mk("stop", false, why), mk("restart", false, why)];
      } else {
        out = [];
      }
      break;
    default:
      out = [];
  }
  if (ctx.kind === "postgres") out = out.filter((a) => a.action === "stop");
  if (ctx.blockedReason) out = out.map((a) => ({ ...a, disabled: true, reason: ctx.blockedReason }));
  return out;
}

/* ---------------- Summaries and the system verdict ---------------- */

/** Anything row-like carrying a backend's lifecycle: a BackendRow
 *  (state/lifecycle wire strings) or a BackendInfo (lifecycle). */
export interface SummarizableBackend {
  state?: string;
  lifecycle?: string;
  enabled?: boolean;
  configured?: boolean;
}

export interface LifecycleSummary {
  /** Enabled, configured backends — the ones expected to run. */
  backendsTotal: number;
  backendsRunning: number;
  backendsTransitioning: number;
  backendsFailed: number;
  /** null when the gateway state is unmeasured (probe failed, sample mode). */
  gateway: LifecycleState | null;
}

/** The one count rule for the sidebar, the dashboard pill and Start-all
 *  bar, and the map pill: enabled + configured backends only. */
export function summarize(input: {
  backends: readonly SummarizableBackend[];
  gateway: LifecycleState | null;
  /** Optional per-backend override (the store's pending overlay). */
  stateOf?: (b: SummarizableBackend) => LifecycleState;
}): LifecycleSummary {
  const stateOf = input.stateOf ?? ((b: SummarizableBackend) => fromWire(b.lifecycle ?? b.state));
  const counted = input.backends.filter((b) => b.enabled !== false && b.configured !== false);
  let running = 0, transitioning = 0, failed = 0;
  for (const b of counted) {
    const s = stateOf(b);
    if (s === "running") running++;
    else if (META[s].isTransition) transitioning++;
    else if (s === "failed") failed++;
  }
  return {
    backendsTotal: counted.length,
    backendsRunning: running,
    backendsTransitioning: transitioning,
    backendsFailed: failed,
    gateway: input.gateway,
  };
}

export interface SystemVerdict {
  /** Dot class vocabulary shared by the dashboard pill and the map pill. */
  dot: DotClass;
  text: string;
  ok: boolean;
}

const DOT_RANK: Record<DotClass, number> = { ok: 0, idle: 1, warn: 2, err: 3 };
const worse = (a: DotClass, b: DotClass): DotClass => (DOT_RANK[b] > DOT_RANK[a] ? b : a);

/** The gateway's part of the verdict: null when it does not degrade it. */
function gatewayVerdict(g: LifecycleState | null): { dot: DotClass; text: string } | null {
  switch (g) {
    case null: case "running": return null;
    case "starting": return { dot: "warn", text: "Gateway starting…" };
    case "restarting": return { dot: "warn", text: "Gateway restarting…" };
    case "stopping": return { dot: "warn", text: "Gateway stopping…" };
    case "stopped": return { dot: "idle", text: "Gateway stopped" };
    case "failed": return { dot: "err", text: "Gateway down" };
    case "degraded": return { dot: "warn", text: "Gateway not responding" };
    case "not_installed": return { dot: "idle", text: "Gateway not installed" };
    default: return { dot: "warn", text: "Gateway state unknown" };
  }
}

/** The one system-verdict rule, shared by the Dashboard header pill and
 *  the Service Map pill. "All systems normal" iff at least one backend
 *  is expected to run, all of them run, telemetry is fresh, no load
 *  error occurred, and the gateway is running (or unmeasured). A
 *  gateway or backend mid-restart reads as a transition (amber), never
 *  as down; an operator-stopped gateway is idle, a crashed one red. */
export function systemVerdict(input: { summary: LifecycleSummary; stale: boolean; error: string | null }): SystemVerdict {
  const { summary: s, stale, error } = input;
  if (error) return { dot: "err", text: "Backend error", ok: false };
  if (s.backendsTotal === 0) return { dot: "idle", text: "No backends configured", ok: false };
  const gw = gatewayVerdict(s.gateway);
  if (s.backendsRunning === s.backendsTotal) {
    if (stale) return { dot: "warn", text: "Telemetry stale", ok: false };
    if (gw) return { dot: gw.dot, text: gw.text, ok: false };
    return { dot: "ok", text: "All systems normal", ok: true };
  }
  let dot: DotClass;
  let text: string;
  if (s.backendsRunning === 0 && s.backendsTransitioning === 0 && s.backendsFailed === 0) {
    dot = "idle";
    text = "All backends stopped";
  } else {
    dot = "warn";
    text = `${s.backendsRunning} / ${s.backendsTotal} backends running`;
    if (s.backendsTransitioning) text += ` · ${s.backendsTransitioning} changing state`;
    if (s.backendsFailed) {
      text += ` · ${s.backendsFailed} failed`;
      dot = "err";
    }
  }
  // A not-running gateway is part of the picture in the partial and
  // all-stopped states too — the pill must not read as if backends
  // were the only problem.
  if (gw) {
    text += ` · ${gw.text.charAt(0).toLowerCase()}${gw.text.slice(1)}`;
    dot = worse(dot, gw.dot);
  }
  return { dot, text, ok: false };
}

/* The shared service-lifecycle module: ONE vocabulary, ONE set of
   renderers, ONE action rule, ONE operation runner and ONE status store
   for every managed service (backends, gateway, PostgreSQL, log
   receiver) on every page and card.

   - Vocabulary, normalizers, META, toStatus, summarize, systemVerdict
     and lifecycleActions are pure and live in lifecycle-state.ts
     (re-exported here; import from this module).
   - Renderers: lifecycleDot / lifecycleChip (+ lifecycleLabel,
     lifecycleColor, lifecycleEdge from the pure half).
   - Actions: renderLifecycleActions emits the Start/Stop/Restart
     buttons; wireLifecycleActions is the one delegated click handler
     that runs them through runLifecycle.
   - runLifecycle: maps a target + action onto the api wrappers, shows
     the transition at once on every surface (store pending overlay),
     and refreshes the store when the operation returns.
   - serviceStore: one poller of list_backends + gateway_status (and
     services_list + log_receiver_status for subscribers that ask),
     fast while anything is in transition, refreshed immediately on the
     backend's "service-lifecycle" / "backend-status" events. */

import {
  gatewayRestart,
  gatewayStop,
  getBackends,
  getGatewayStatus,
  isTauri,
  logReceiverRestart,
  logReceiverStart,
  logReceiverStatus,
  logReceiverStop,
  postgresStop,
  restartBackend,
  runWizardStep,
  servicesList,
  startBackend,
  stopBackend,
  wizardBusyMessage,
} from "./api";
import type { BackendRow, CallResult, GatewayStatusData, LogReceiverInfo, ServiceRow, ServicesInfo } from "./api";
import { esc } from "./html";
import {
  META,
  elapsedSince,
  fromScm,
  fromWire,
  lifecycleActions,
  lifecycleLabel,
  summarize,
} from "./lifecycle-state";
import type {
  LifecycleAction,
  LifecycleActionCtx,
  LifecycleState,
  LifecycleSummary,
  TransitionKind,
  TransitionWire,
} from "./lifecycle-state";
import { promptForUnarmedMcpCredentials } from "./mcp-prompt";
import { mockBackendRows, mockGatewayStatus } from "./mock";

export * from "./lifecycle-state";

/* ---------------- Renderers ---------------- */

/** `<span class="dot …">` for a lifecycle state. */
export function lifecycleDot(s: LifecycleState): string {
  return `<span class="dot ${META[s].dot}"></span>`;
}

/** Tooltip text for a chip: the transition's origin and age, or the
 *  failure reason, plus any caller detail. */
function chipTitle(s: LifecycleState, opts?: LifecycleChipOpts): string {
  const parts: string[] = [];
  const t = opts?.transition;
  if (t) {
    const age = elapsedSince(t.since);
    parts.push(`${t.kind} — requested by ${t.origin || "unknown"}${age ? `, ${age} ago` : ""}`);
  }
  if ((s === "failed" || s === "degraded") && opts?.failure) parts.push(opts.failure);
  if (opts?.detail) parts.push(opts.detail);
  return parts.join("\n");
}

export interface LifecycleChipOpts {
  /** Extra tooltip text (pid, port, service name, …). */
  detail?: string;
  /** The transition in flight: its origin and age go in the tooltip. */
  transition?: TransitionWire | null;
  /** Failure reason shown in the tooltip of a failed/degraded chip. */
  failure?: string | null;
}

/** `<span class="chip status-tag …">label</span>` for a lifecycle state.
 *  A string second argument is tooltip detail (the short form). */
export function lifecycleChip(s: LifecycleState, opts?: string | LifecycleChipOpts): string {
  const o: LifecycleChipOpts | undefined = typeof opts === "string" ? { detail: opts } : opts;
  const title = chipTitle(s, o);
  return `<span class="chip status-tag ${META[s].tag}" data-lc-state="${s}"${title ? ` title="${esc(title)}"` : ""}>${esc(META[s].label)}</span>`;
}

/* ---------------- Targets ---------------- */

export type LifecycleTarget =
  | { kind: "backend"; id: string }
  | { kind: "gateway" }
  | { kind: "postgres" }
  | { kind: "log-receiver" };

/** The registry key of a target — the same keys the backend uses
 *  ("backend:<id>", "gateway", "postgres", "log-receiver"). */
export function targetKey(t: LifecycleTarget): string {
  return t.kind === "backend" ? `backend:${t.id}` : t.kind;
}

/** Inverse of targetKey; null for an unrecognized key. */
export function parseTargetKey(key: string): LifecycleTarget | null {
  if (key.startsWith("backend:") && key.length > "backend:".length) return { kind: "backend", id: key.slice("backend:".length) };
  if (key === "gateway" || key === "postgres" || key === "log-receiver") return { kind: key };
  return null;
}

/* ---------------- Action buttons ---------------- */

export interface RenderActionsOpts {
  /** Extra classes on every button (default "btn"). */
  className?: string;
  /** Show only these actions (e.g. a compact card that has no Restart). */
  only?: LifecycleAction[];
  /** Per-action label override. */
  labels?: Partial<Record<LifecycleAction, string>>;
}

/** Why a target's Start/Stop/Restart is blocked by a running activation
 *  apply (undefined when it is not): the apply restarts backends and the
 *  gateway itself, so an operator action meanwhile would make the
 *  apply's own operation fail as busy. `applying` defaults to the shared
 *  store's gateway_status.activation_applying. */
export function applyBlockedReason(
  target: LifecycleTarget,
  applying: boolean = Boolean(serviceStore.get().gateway?.activation_applying),
): string | undefined {
  if (!applying) return undefined;
  return target.kind === "gateway" || target.kind === "backend" ? "An activation apply is running" : undefined;
}

/** Start/Stop/Restart buttons for a target in a given state, from the
 *  shared lifecycleActions rule. Disabled buttons carry their reason as
 *  the tooltip; every gateway and backend button is blocked while an
 *  activation apply runs (applyBlockedReason). Wire them with
 *  wireLifecycleActions on an ancestor. */
export function renderLifecycleActions(
  target: LifecycleTarget,
  s: LifecycleState,
  ctx: Omit<LifecycleActionCtx, "kind"> = {},
  opts: RenderActionsOpts = {},
): string {
  const key = targetKey(target);
  const blockedReason = ctx.blockedReason ?? applyBlockedReason(target);
  return lifecycleActions(s, { ...ctx, kind: target.kind, ...(blockedReason ? { blockedReason } : {}) })
    .filter((a) => !opts.only || opts.only.includes(a.action))
    .map((a) => {
      const label = opts.labels?.[a.action] ?? a.label;
      const title = a.reason ? ` title="${esc(a.reason)}"` : "";
      return `<button type="button" class="${esc(opts.className ?? "btn")}" data-lc-act="${a.action}" data-lc-target="${esc(key)}"${a.disabled ? " disabled" : ""}${title}>${esc(label)}</button>`;
    })
    .join("");
}

export interface WireActionsOpts {
  /** The origin recorded in the service's lifecycle log (default
   *  root.dataset.lcOrigin, else "ui"). */
  origin?: string;
  /** Drain in-flight requests before a gateway restart. */
  drain?: boolean;
  /** Called before an action runs; return false to cancel it (e.g. a
   *  confirmation). */
  confirm?: (target: LifecycleTarget, action: LifecycleAction) => boolean | Promise<boolean>;
}

/** The one delegated click handler for renderLifecycleActions buttons
 *  under `root`. Gateway start/restart run the MCP credential prompt
 *  first. `onDone` receives each result (render its message). Returns
 *  a function that removes the handler. */
export function wireLifecycleActions(
  root: HTMLElement,
  onDone?: (result: LifecycleResult, target: LifecycleTarget, action: LifecycleAction) => void,
  opts: WireActionsOpts = {},
): () => void {
  const handler = async (ev: Event) => {
    const el = (ev.target as HTMLElement | null)?.closest?.("[data-lc-act]") as HTMLButtonElement | null;
    if (!el || !root.contains(el) || el.disabled) return;
    const action = el.dataset.lcAct as LifecycleAction | undefined;
    const target = parseTargetKey(el.dataset.lcTarget ?? "");
    if (!target || (action !== "start" && action !== "stop" && action !== "restart")) return;
    ev.preventDefault();
    if (opts.confirm && !(await opts.confirm(target, action))) return;
    el.disabled = true;
    const result = await runLifecycle(target, action, {
      origin: opts.origin ?? root.dataset.lcOrigin ?? "ui",
      drain: opts.drain,
      promptMcp: target.kind === "gateway" && action !== "stop",
    });
    if (el.isConnected) el.disabled = false;
    onDone?.(result, target, action);
  };
  root.addEventListener("click", handler);
  return () => root.removeEventListener("click", handler);
}

/* ---------------- Operation runner ---------------- */

export interface LifecycleRunOpts {
  /** Who asked: ui (default), dashboard, activation, bench, optimize, … */
  origin?: string;
  /** Stop/restart a backend gracefully (drain first; default true). */
  graceful?: boolean;
  /** Gateway restart: drain in-flight requests first. */
  drain?: boolean;
  /** Gateway start/restart: offer to store missing MCP credentials first. */
  promptMcp?: boolean;
  /** Backend start/restart: the WDDM-overcommit confirmation. */
  allowWddmOvercommit?: boolean;
  /** Backend restart: wait up to this long for /health. */
  waitReadySecs?: number;
  /** Gateway stop: stop PostgreSQL too (explicit only). */
  stopPostgres?: boolean;
}

export interface LifecycleResult {
  ok: boolean;
  message: string;
  manualCommand?: string | null;
  /** Another lifecycle operation holds the service. */
  busy?: boolean;
  /** The operator cancelled (MCP prompt or confirm); nothing ran. */
  cancelled?: boolean;
}

const PENDING_KIND: Record<LifecycleAction, TransitionKind> = { start: "starting", stop: "stopping", restart: "restarting" };

function targetName(t: LifecycleTarget): string {
  switch (t.kind) {
    case "backend": return `Backend ${t.id}`;
    case "gateway": return "Gateway";
    case "postgres": return "PostgreSQL";
    case "log-receiver": return "Log receiver";
  }
}

const PAST: Record<LifecycleAction, string> = { start: "started", stop: "stopped", restart: "restarted" };

/** A failed call's result: busy when the server's refusal says another
 *  operation holds the service. */
function callFailure(t: LifecycleTarget, action: LifecycleAction, r: CallResult<unknown> & { busy?: boolean }): LifecycleResult {
  const error = r.error ?? "unknown error";
  if (r.busy) return { ok: false, busy: true, message: `${targetName(t)}: ${error}` };
  return { ok: false, message: `${targetName(t)} ${action} failed: ${error}` };
}

async function dispatchLifecycle(t: LifecycleTarget, action: LifecycleAction, opts: LifecycleRunOpts): Promise<LifecycleResult> {
  const origin = opts.origin ?? "ui";
  const name = targetName(t);
  switch (t.kind) {
    case "backend": {
      const bo = { origin, graceful: opts.graceful, allowWddmOvercommit: opts.allowWddmOvercommit, waitReadySecs: opts.waitReadySecs };
      const r = action === "start" ? await startBackend(t.id, bo) : action === "stop" ? await stopBackend(t.id, bo) : await restartBackend(t.id, bo);
      if (r.error) return callFailure(t, action, r);
      if (!r.live) return { ok: false, message: `${name}: not available in sample mode` };
      return { ok: true, message: `${name} ${action === "stop" ? "stopped" : `${action} requested`}.` };
    }
    case "gateway": {
      if (action === "stop") {
        const r = await gatewayStop({ origin, stopPostgres: opts.stopPostgres });
        if (r.error) return callFailure(t, action, r);
        const pg = r.data.postgres_stopped ? " PostgreSQL stopped too." : "";
        return { ok: true, message: (r.data.stopped ? "Gateway stopped." : "Gateway was not running.") + pg };
      }
      if (action === "restart") {
        const r = await gatewayRestart({ origin, drain: opts.drain });
        if (r.data.busy) return { ok: false, busy: true, message: wizardBusyMessage(r.data, r.data.message ?? r.error ?? "Another gateway operation is in progress.") };
        if (r.error && !r.data.outcome) return callFailure(t, action, r);
        const msg = r.data.message ?? r.data.outcome?.message ?? "";
        return r.data.ok
          ? { ok: true, message: `Gateway restarted${msg ? ` — ${msg}` : "."}` }
          : { ok: false, message: `Gateway restart failed${msg ? `: ${msg}` : "."}`, manualCommand: r.data.manual_command };
      }
      // Start: the start-gateway setup step (it stops a running gateway
      // itself, so this is also the old "restart"; PostgreSQL is adopted,
      // not restarted).
      const r = await runWizardStep("start-gateway", { origin });
      if (r.data.busy) {
        return { ok: false, busy: true, message: wizardBusyMessage(r.data, "A setup step or gateway operation is already running — wait for it to finish, then try again.") };
      }
      if (r.error && !r.data.outcome) return callFailure(t, action, r);
      const o = r.data.outcome;
      const msg = o?.message ?? r.error ?? "";
      return r.data.ok
        ? { ok: true, message: `Gateway started${msg ? ` — ${msg}` : "."}` }
        : { ok: false, message: `Gateway start ${o?.state ?? "failed"}${msg ? `: ${msg}` : "."}`, manualCommand: o?.manual_command ?? null };
    }
    case "postgres": {
      if (action !== "stop") {
        return { ok: false, message: "PostgreSQL starts with the gateway — start or restart the gateway instead." };
      }
      const r = await postgresStop({ origin });
      if (r.error) return callFailure(t, action, r);
      return { ok: true, message: r.data.stopped ? "PostgreSQL stopped." : "PostgreSQL was not running." };
    }
    case "log-receiver": {
      const r = action === "start" ? await logReceiverStart({ origin }) : action === "stop" ? await logReceiverStop({ origin }) : await logReceiverRestart({ origin });
      if (r.error) return callFailure(t, action, r);
      if (!r.data.ok) return { ok: false, message: `${name} ${action} failed${r.data.detail ? `: ${r.data.detail}` : "."}` };
      return { ok: true, message: `${name} ${PAST[action]}${r.data.detail ? ` — ${r.data.detail}` : "."}` };
    }
  }
}

/** Run one lifecycle operation. The transition shows at once on every
 *  surface (the store's pending overlay) and survives navigation; the
 *  overlay clears when the call returns, after a store refresh, so the
 *  server's view takes over without a flash of the old state. Busy and
 *  failed outcomes come back as `ok: false` (never thrown). */
export async function runLifecycle(target: LifecycleTarget, action: LifecycleAction, opts: LifecycleRunOpts = {}): Promise<LifecycleResult> {
  if (opts.promptMcp && target.kind === "gateway" && action !== "stop") {
    const proceed = await promptForUnarmedMcpCredentials();
    if (!proceed) return { ok: false, cancelled: true, message: "Gateway start cancelled at the MCP credential prompt." };
  }
  const key = targetKey(target);
  serviceStore.markPending(key, PENDING_KIND[action]);
  let result: LifecycleResult;
  try {
    result = await dispatchLifecycle(target, action, opts);
  } catch (e) {
    result = { ok: false, message: `${targetName(target)} ${action} failed: ${e instanceof Error ? e.message : String(e)}` };
  }
  try {
    await serviceStore.refresh();
  } finally {
    serviceStore.clearPending(key);
  }
  return result;
}

/* ---------------- Service store ---------------- */

export interface PendingEntry {
  kind: TransitionKind;
  /** Date.now() when the operation was asked for. */
  since: number;
}

export interface ServiceSnapshot {
  /** False in sample mode (mock data). */
  live: boolean;
  /** Date.now() of the last completed refresh; 0 before the first. */
  updatedAt: number;
  backends: BackendRow[];
  backendsError: string | null;
  /** Null until the first refresh, or when gateway_status failed. */
  gateway: GatewayStatusData | null;
  gatewayError: string | null;
  /** Only fetched while a subscriber asked for services. */
  services: ServicesInfo | null;
  logReceiver: LogReceiverInfo | null;
  /** Local operations in flight, by target key. */
  pending: ReadonlyMap<string, PendingEntry>;
}

export type ServiceListener = (snap: ServiceSnapshot) => void;

/** Poll cadence: idle, and while anything is in transition. */
export const STORE_IDLE_MS = 5000;
export const STORE_FAST_MS = 1500;
/** A pending entry older than this is dropped (a lost call must not
 *  pin a card in "restarting…" forever). Gateway starts can repair a
 *  venv, so this is generous. */
const PENDING_MAX_MS = 30 * 60_000;

/** The pending overlay rule: a local pending operation shows its kind
 *  until the server itself reports a transition (which then wins: it
 *  names who asked and since when). */
export function overlayState(wire: LifecycleState, pending: PendingEntry | undefined): LifecycleState {
  if (!pending) return wire;
  if (META[wire].isTransition) return wire;
  return pending.kind;
}

/** Whether a snapshot shows anything in motion (fast polling). */
export function snapshotBusy(s: Pick<ServiceSnapshot, "backends" | "gateway" | "services" | "logReceiver" | "pending">): boolean {
  if (s.pending.size > 0) return true;
  if (s.backends.some((b) => b.transition || META[fromWire(b.lifecycle ?? b.state)].isTransition)) return true;
  const g = s.gateway;
  if (g && (g.transition || g.activation_applying || META[g.state].isTransition || g.postgres?.transition)) return true;
  if (s.logReceiver && (s.logReceiver.transition || META[logReceiverState(s.logReceiver)].isTransition)) return true;
  if (s.services?.services.some((r) => r.transition || META[serviceRowState(r)].isTransition)) return true;
  return false;
}

/** A services_list row's lifecycle state (`lifecycle`, else the SCM state). */
export function serviceRowState(r: ServiceRow): LifecycleState {
  if (r.lifecycle !== undefined) return fromWire(r.lifecycle);
  return r.registered ? fromScm(r.state) : "not_installed";
}

/** The log receiver's lifecycle state (`lifecycle`, else the SCM state). */
export function logReceiverState(r: LogReceiverInfo): LifecycleState {
  if (r.lifecycle !== undefined) return fromWire(r.lifecycle);
  return r.registered ? fromScm(r.state) : "not_installed";
}

class ServiceStore {
  private snap: ServiceSnapshot = {
    live: isTauriSafe(),
    updatedAt: 0,
    backends: [],
    backendsError: null,
    gateway: null,
    gatewayError: null,
    services: null,
    logReceiver: null,
    pending: new Map(),
  };
  private pendingMap = new Map<string, PendingEntry>();
  private listeners = new Map<ServiceListener, { services: boolean }>();
  private timer: ReturnType<typeof setTimeout> | null = null;
  private inflight: Promise<void> | null = null;
  private again = false;
  private eventsWired = false;
  private eventDebounce: ReturnType<typeof setTimeout> | null = null;

  /** The latest snapshot (also before the first refresh: empty). */
  get(): ServiceSnapshot {
    return this.snap;
  }

  /** Subscribe; the listener gets the current snapshot at once when
   *  one exists, then every refresh. `services: true` adds
   *  services_list + log_receiver_status to the poll while subscribed.
   *  Polling runs only while someone is subscribed. */
  subscribe(fn: ServiceListener, opts: { services?: boolean } = {}): () => void {
    const firstServices = opts.services && !this.wantsServices();
    this.listeners.set(fn, { services: Boolean(opts.services) });
    this.wireEvents();
    if (this.snap.updatedAt > 0) {
      try { fn(this.snap); } catch (e) { console.error("service store listener failed", e); }
    }
    if (this.snap.updatedAt === 0 || firstServices) void this.refresh();
    else this.schedule();
    return () => {
      this.listeners.delete(fn);
      if (this.listeners.size === 0) this.stopTimer();
    };
  }

  /** Re-poll now (coalesced: a call during a poll runs one more poll
   *  after it, and both callers resolve after that). */
  refresh(): Promise<void> {
    if (this.inflight) {
      this.again = true;
      return this.inflight;
    }
    this.inflight = (async () => {
      try {
        do {
          this.again = false;
          await this.poll();
        } while (this.again);
      } finally {
        this.inflight = null;
        this.schedule();
      }
    })();
    return this.inflight;
  }

  /** Mark a local operation in flight: every surface shows `kind` for
   *  the target at once (see overlayState). */
  markPending(key: string, kind: TransitionKind): void {
    this.pendingMap.set(key, { kind, since: Date.now() });
    this.publish();
    this.schedule();
  }

  clearPending(key: string): void {
    if (this.pendingMap.delete(key)) this.publish();
  }

  /** The state to render for a target key given the wire state: the
   *  pending overlay until the server reports a transition. */
  effective(key: string, wireState: LifecycleState | string | null | undefined): LifecycleState {
    const wire = typeof wireState === "string" ? fromWire(wireState) : wireState ?? "unknown";
    return overlayState(wire, this.pendingMap.get(key));
  }

  /** The transition to describe for a target: the server's, else a
   *  synthesized one for a local pending operation. */
  transitionOf(key: string, wire: TransitionWire | null | undefined): TransitionWire | null {
    if (wire) return wire;
    const p = this.pendingMap.get(key);
    return p ? { kind: p.kind, origin: "ui", since: new Date(p.since).toISOString() } : null;
  }

  /** Effective state of a backend row. */
  backendState(row: BackendRow): LifecycleState {
    return this.effective(`backend:${row.id}`, row.lifecycle ?? row.state);
  }

  /** Effective gateway state; null when unmeasured (no snapshot yet or
   *  the gateway_status call failed). */
  gatewayState(): LifecycleState | null {
    const g = this.snap.gateway;
    if (!g) return this.pendingMap.has("gateway") ? this.effective("gateway", "unknown") : null;
    return this.effective("gateway", g.state);
  }

  /** Effective PostgreSQL state; null when not reported. */
  postgresState(): LifecycleState | null {
    const p = this.snap.gateway?.postgres;
    return p ? this.effective("postgres", p.state) : null;
  }

  /** Effective log receiver state; null when not fetched. */
  logReceiverState(): LifecycleState | null {
    const r = this.snap.logReceiver;
    return r ? this.effective("log-receiver", logReceiverState(r)) : null;
  }

  /** The shared counts (enabled + configured backends, gateway state)
   *  with the pending overlay applied. */
  summary(): LifecycleSummary {
    return summarize({
      backends: this.snap.backends,
      gateway: this.gatewayState(),
      stateOf: (b) => this.backendState(b as BackendRow),
    });
  }

  private wantsServices(): boolean {
    for (const o of this.listeners.values()) if (o.services) return true;
    return false;
  }

  private async poll(): Promise<void> {
    const withServices = this.wantsServices();
    let next: Omit<ServiceSnapshot, "pending">;
    if (!isTauriSafe()) {
      const lr = withServices ? (await logReceiverStatus()).data : null;
      const sv = withServices ? (await servicesList()).data : null;
      next = {
        live: false, updatedAt: Date.now(),
        backends: mockBackendRows(), backendsError: null,
        gateway: mockGatewayStatus(), gatewayError: null,
        services: sv, logReceiver: lr,
      };
    } else {
      const [b, g, sv, lr] = await Promise.all([
        getBackends(),
        getGatewayStatus(),
        withServices ? servicesList() : Promise.resolve(null),
        withServices ? logReceiverStatus() : Promise.resolve(null),
      ]);
      next = {
        live: true,
        updatedAt: Date.now(),
        // A failed list keeps the last rows (state unknown, not "all
        // stopped"); the error rides along for the caller's banner.
        backends: b.error ? this.snap.backends : b.data,
        backendsError: b.error ?? null,
        gateway: g.error ? null : g.data,
        gatewayError: g.error ?? null,
        services: sv ? (sv.error ? this.snap.services : sv.data) : withServices ? this.snap.services : null,
        logReceiver: lr ? (lr.error ? this.snap.logReceiver : lr.data) : withServices ? this.snap.logReceiver : null,
      };
    }
    this.expirePending();
    this.snap = { ...next, pending: new Map(this.pendingMap) };
    this.emit();
  }

  private expirePending(): void {
    const now = Date.now();
    for (const [k, p] of this.pendingMap) if (now - p.since > PENDING_MAX_MS) this.pendingMap.delete(k);
  }

  /** Republish the current snapshot with the current pending map. */
  private publish(): void {
    this.snap = { ...this.snap, pending: new Map(this.pendingMap) };
    if (this.snap.updatedAt > 0) this.emit();
  }

  private emit(): void {
    for (const fn of [...this.listeners.keys()]) {
      try { fn(this.snap); } catch (e) { console.error("service store listener failed", e); }
    }
  }

  private stopTimer(): void {
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }

  private schedule(): void {
    this.stopTimer();
    if (this.listeners.size === 0 || this.inflight) return;
    const ms = snapshotBusy(this.snap) ? STORE_FAST_MS : STORE_IDLE_MS;
    this.timer = setTimeout(() => {
      this.timer = null;
      void this.refresh();
    }, ms);
  }

  /** Subscribe once to the backend's lifecycle events; each triggers
   *  a (200 ms debounced) refresh while anyone is subscribed. */
  private wireEvents(): void {
    if (this.eventsWired) return;
    this.eventsWired = true;
    const events = typeof window !== "undefined" ? window.__TAURI__?.event : undefined;
    if (!events || typeof events.listen !== "function") return;
    const onEvent = () => {
      if (this.listeners.size === 0) return;
      if (this.eventDebounce !== null) clearTimeout(this.eventDebounce);
      this.eventDebounce = setTimeout(() => {
        this.eventDebounce = null;
        void this.refresh();
      }, 200);
    };
    for (const name of ["service-lifecycle", "backend-status"]) {
      try {
        events.listen(name, onEvent).catch((e) => console.warn(`service store: could not listen to ${name}`, e));
      } catch (e) {
        console.warn(`service store: could not listen to ${name}`, e);
      }
    }
  }
}

function isTauriSafe(): boolean {
  try {
    return typeof window !== "undefined" && isTauri();
  } catch {
    return false;
  }
}

/** The one service status store. */
export const serviceStore = new ServiceStore();

/** Convenience: the label for a target's effective state with its
 *  transition ("restarting… (activation, 12 s)"). */
export function storeLabel(key: string, wireState: LifecycleState | string | null | undefined, wireTransition?: TransitionWire | null): string {
  return lifecycleLabel(serviceStore.effective(key, wireState), serviceStore.transitionOf(key, wireTransition));
}

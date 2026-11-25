import { fromWire, isBusyError } from "../lifecycle-state";
import type { FailureWire, LifecycleState, TransitionWire } from "../lifecycle-state";
import { mockGatewayStatus } from "../mock";
import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";
import type { WizardStepOutcome } from "./wizard";

/* ---------------- Gateway ---------------- */

export interface GatewayGroup {
  name: string;
  members: string[];
  strategy: string;
  fallbacks: string[];
  /** The operator's stated purpose for the group (Optimize design
   *  §2.3), carried so a groups patch writes it back verbatim
   *  instead of resetting it to the serde default. */
  purpose: string;
  /** Derived by the backend (manager-gateway), never stored: the
   *  group is enabled iff at least one member backend is enabled,
   *  and only enabled groups reach the generated gateway config. */
  enabled: boolean;
  /** How many member backends are enabled — the basis of `enabled`. */
  enabled_members: number;
}

/** A typed routing rule's behavior (manager-config's
 *  RoutingRuleKind, routing-rules design §6.2). Only the
 *  context-overflow kind exists in v1. Absent/null on legacy
 *  fragment-only rules, which are inert and render read-only. */
export interface GatewayRuleKind {
  type: "context-overflow";
  target_group: string;
  /** Advisory threshold as a fraction of the serving group's
   *  hard window; ignored when optimal_tokens is set. */
  optimal_fraction: number;
  /** Absolute advisory threshold in tokens, or null for the
   *  fraction default (half the serving group's window). */
  optimal_tokens: number | null;
  /** Output tokens assumed when a request sets no max_tokens. */
  output_reserve_tokens: number;
  advisory: boolean;
}

export interface GatewayRule {
  id: string;
  description: string;
  source: string;
  /** Serde default true on payloads that predate the field. */
  enabled: boolean;
  /** The typed behavior, or null for a legacy fragment-only rule. */
  kind: GatewayRuleKind | null;
  /** The LiteLLM config fragment this rule injects, carried by the
   *  get_gateway_config payload; shown in the rule's Fragment
   *  expandable. Absent on payloads that predate the field. */
  litellm_fragment?: string;
  /** The fragment's untouched raw value from the payload, carried
   *  so a save through update_gateway_config writes back exactly
   *  what was stored (the display form above stringifies objects).
   *  Undefined when the payload carried no fragment. */
  fragment_raw?: unknown;
}

export interface GatewayGroup {
  name: string;
  members: string[];
  strategy: string;
  fallbacks: string[];
  /** The operator's stated purpose for the group (Optimize design
   *  §2.3), carried so a groups patch writes it back verbatim
   *  instead of resetting it to the serde default. */
  purpose: string;
  /** Derived by the backend (manager-gateway), never stored: the
   *  group is enabled iff at least one member backend is enabled,
   *  and only enabled groups reach the generated gateway config. */
  enabled: boolean;
  /** How many member backends are enabled — the basis of `enabled`. */
  enabled_members: number;
}

/** The OpenRouter cloud fallback tier (manager-config's
 *  CloudTierConfig). The key itself is never in the config — only the
 *  Credential Manager reference name. */
export interface GatewayOpenRouter {
  enabled: boolean;
  cred_ref: string | null;
  daily_cap_usd: number;
  model_allowlist: string[];
}

export interface GatewayConfigData {
  port: number;
  groups: GatewayGroup[];
  routing_rules: GatewayRule[];
  request_log_len: number;
  openrouter: GatewayOpenRouter | null;
  /** The one global routing strategy LiteLLM will apply, computed by
   *  the backend with the config generator's own rule
   *  (manager-gateway effective_global_strategy) — never re-derived
   *  here. Null when no gateway config would be generated (or an
   *  older backend did not send it). */
  effective_strategy: string | null;
}

/** get_gateway_config — LiteLLM gateway configuration from AppConfig.
 *  The command serializes the whole GatewayConfig; the openrouter tier
 *  is mapped out explicitly so the view can render it (it was silently
 *  dropped by the earlier typing). */
export async function getGatewayConfig(): Promise<CallResult<GatewayConfigData | null>> {
  if (!isTauri()) return { data: null, live: false };
  try {
    const raw = await invoke<Record<string, unknown>>("get_gateway_config");
    const or = raw.openrouter as Partial<GatewayOpenRouter> | null | undefined;
    const data: GatewayConfigData = {
      port: typeof raw.port === "number" ? raw.port : 0,
      groups: Array.isArray(raw.groups) ? (raw.groups as Array<Partial<GatewayGroup>>).map((g) => ({
        name: typeof g.name === "string" ? g.name : "",
        members: Array.isArray(g.members) ? g.members : [],
        strategy: typeof g.strategy === "string" ? g.strategy : "",
        fallbacks: Array.isArray(g.fallbacks) ? g.fallbacks : [],
        purpose: typeof g.purpose === "string" ? g.purpose : "",
        enabled: typeof g.enabled === "boolean" ? g.enabled : true,
        enabled_members: typeof g.enabled_members === "number"
          ? g.enabled_members
          : (Array.isArray(g.members) ? g.members.length : 0),
      })) : [],
      routing_rules: Array.isArray(raw.routing_rules) ? (raw.routing_rules as Array<Partial<GatewayRule> & Record<string, unknown>>).map((r) => ({
        id: typeof r.id === "string" ? r.id : "",
        description: typeof r.description === "string" ? r.description : "",
        source: typeof r.source === "string" ? r.source : "",
        enabled: typeof r.enabled === "boolean" ? r.enabled : true,
        kind: r.kind && typeof r.kind === "object" && (r.kind as GatewayRuleKind).type === "context-overflow"
          ? {
              type: "context-overflow" as const,
              target_group: typeof (r.kind as GatewayRuleKind).target_group === "string" ? (r.kind as GatewayRuleKind).target_group : "",
              optimal_fraction: typeof (r.kind as GatewayRuleKind).optimal_fraction === "number" ? (r.kind as GatewayRuleKind).optimal_fraction : 0.5,
              optimal_tokens: typeof (r.kind as GatewayRuleKind).optimal_tokens === "number" ? (r.kind as GatewayRuleKind).optimal_tokens : null,
              output_reserve_tokens: typeof (r.kind as GatewayRuleKind).output_reserve_tokens === "number" ? (r.kind as GatewayRuleKind).output_reserve_tokens : 1024,
              advisory: (r.kind as GatewayRuleKind).advisory !== false,
            }
          : null,
        // The fragment is an arbitrary JSON value in the stored
        // config; objects render as their JSON text.
        litellm_fragment: typeof r.litellm_fragment === "string"
          ? r.litellm_fragment
          : r.litellm_fragment != null
            ? JSON.stringify(r.litellm_fragment)
            : undefined,
        fragment_raw: r.litellm_fragment === undefined ? undefined : r.litellm_fragment,
      })) : [],
      request_log_len: typeof raw.request_log_len === "number" ? raw.request_log_len : 0,
      effective_strategy: typeof raw.effective_strategy === "string" ? raw.effective_strategy : null,
      openrouter: or && typeof or === "object"
        ? {
            enabled: Boolean(or.enabled),
            cred_ref: typeof or.cred_ref === "string" ? or.cred_ref : null,
            daily_cap_usd: typeof or.daily_cap_usd === "number" ? or.daily_cap_usd : 0,
            model_allowlist: Array.isArray(or.model_allowlist) ? or.model_allowlist : [],
          }
        : null,
    };
    return { data, live: true };
  } catch (e) {
    return { data: null, live: true, error: errOf(e) };
  }
}

export interface GatewayStatusData {
  port: number;
  /** Enabled groups only: a group counts when at least one of its
   *  member backends is enabled — the groups the gateway actually
   *  serves, not the configured total (tracker #11). */
  groups: number;
  routing_rules: number;
  /** Live probe result: true when the gateway answered on its port.
   *  Kept for older consumers; render `state`, never this. */
  up: boolean;
  /** The gateway lifecycle state (the shared gateway view): a restart
   *  reads "restarting", an operator stop "stopped", a crash "failed",
   *  alive-but-not-answering "degraded". Derived from `up` on payloads
   *  from before the field existed. */
  state: LifecycleState;
  /** The gateway operation in flight ({kind, origin, since}), or null. */
  transition: TransitionWire | null;
  /** What the operator last asked for, or null when never recorded. */
  intent: "running" | "stopped" | null;
  /** The live gateway pid (pid file, else the port owner), or null. */
  pid: number | null;
  /** True when the gateway runs as a Windows service. */
  service_managed: boolean;
  /** The last unexpected failure (crash, failed start), or null. */
  failure: FailureWire | null;
  /** The app-managed PostgreSQL server, or null on payloads from
   *  before the field existed. */
  postgres: PostgresStatusData | null;
  /** The server-side activation apply in flight, or null when idle.
   *  Status surfaces keep the plan visible ("Applying…") while set. */
  activation_applying: ActivationProgress | null;
  /** Routing-test traces recorded this session (dry-run decisions from
   *  gateway_test_routing). Live requests are not logged. */
  routing_log_len: number;
  /** Whether the OpenRouter credential reference resolves to a
   *  credential present in Windows Credential Manager: true = present,
   *  false = reference set but not found, null = no reference is
   *  configured (or the backend did not report presence). A presence
   *  check only — the credential value never leaves the backend. */
  openrouter_cred_present: boolean | null;
  /** The configured bind address (0.0.0.0 = all interfaces,
   *  127.0.0.1 = this computer only). */
  bind_host: string;
  /** The workstation's primary LAN address when bound beyond
   *  loopback, for the client endpoint shown to other machines. */
  lan_ip: string | null;
  /** True when saved configuration changes are waiting on a gateway
   *  restart (an activation plan is pending). */
  activation_pending: boolean;
  /** How many journaled changes the pending plan carries. */
  activation_changes: number;
  /** The LiteLLM version recorded by the last gateway spawn (MCP
   *  design §4.3); null when no spawn has recorded one — rendered
   *  as "not measured", never as a guess. */
  litellm_version: string | null;
  /** OpenRouter daily spend vs cap (spec §7), from the app's spend
   *  ledger; absent/null when the backend did not report it. */
  openrouter_spend?: OpenRouterSpend | null;
  /** The port the live gateway process listens on; null/absent when it
   *  is not running or the payload predates the field. `port` is the
   *  SAVED port. */
  running_port?: number | null;
  /** The saved port differs from the running one. */
  port_pending?: boolean;
  /** The global strategy the running gateway applies, when reported. */
  running_strategy?: string | null;
  /** Per group: the strategy the running gateway applies. */
  applied_strategies?: Record<string, string>;
}

/** Per-group strategies as reported by the running gateway: a map, or a
 *  list of rows naming the group and its applied strategy. */
export function parseAppliedStrategies(raw: unknown): Record<string, string> | undefined {
  const out: Record<string, string> = {};
  if (Array.isArray(raw)) {
    for (const r of raw as Record<string, unknown>[]) {
      const name = r?.name ?? r?.group;
      const s = r?.applied_strategy ?? r?.strategy;
      if (typeof name === "string" && typeof s === "string") out[name] = s;
    }
  } else if (raw && typeof raw === "object") {
    for (const [k, v] of Object.entries(raw)) if (typeof v === "string") out[k] = v;
  }
  return Object.keys(out).length ? out : undefined;
}

/** gateway_status.openrouter_spend. The cap itself is enforced inside
 *  the gateway (LiteLLM provider budget, 1-day window); this is the
 *  app's measurement for the banner. */
export interface OpenRouterSpend {
  enabled: boolean;
  /** The generated gateway config carries OpenRouter deployments. */
  tier_active: boolean;
  spent_today_usd: number;
  cap_usd: number;
  /** spent / cap; null when the cap is zero. */
  fraction: number | null;
  /** No OpenRouter traffic will be sent (cap reached, or a zero cap). */
  capped: boolean;
  /** Spend is at or above 80% of the cap. */
  warn: boolean;
}

function parseOpenRouterSpend(raw: unknown): OpenRouterSpend | null {
  if (!raw || typeof raw !== "object") return null;
  const r = raw as Record<string, unknown>;
  const num = (v: unknown): number => (typeof v === "number" && Number.isFinite(v) ? v : 0);
  return {
    enabled: Boolean(r.enabled),
    tier_active: Boolean(r.tier_active),
    spent_today_usd: num(r.spent_today_usd),
    cap_usd: num(r.cap_usd),
    fraction: typeof r.fraction === "number" && Number.isFinite(r.fraction) ? r.fraction : null,
    capped: Boolean(r.capped),
    warn: Boolean(r.warn),
  };
}

/** Banner HTML for the OpenRouter spend cap — shown on the Dashboard
 *  and Gateway views when the cap is reached or spend is at ≥80% of
 *  it; empty otherwise. Uses the shared `.banner` pattern. */
export function openRouterSpendBannerHtml(spend: OpenRouterSpend | null | undefined, warnIcon: string): string {
  if (!spend || !spend.enabled) return "";
  const usd = (v: number) => `$${v.toFixed(2)}`;
  const esc = (s: string) => s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c] as string);
  if (spend.capped) {
    const why = spend.cap_usd > 0
      ? `today's OpenRouter spend ${usd(spend.spent_today_usd)} reached the ${usd(spend.cap_usd)} daily cap`
      : "the OpenRouter daily cap is $0.00";
    return `<div class="banner" id="openrouterSpendBanner" role="status" title="${esc("Cloud fallbacks are disabled: the gateway's provider budget filters OpenRouter out, so requests stay local (fail closed). The cap resets after a day; raise it on the Gateway view's OpenRouter tier.")}">${warnIcon} OpenRouter fallbacks disabled — ${why}. Requests stay local.</div>`;
  }
  if (spend.warn) {
    return `<div class="banner" id="openrouterSpendBanner" role="status">${warnIcon} OpenRouter spend ${usd(spend.spent_today_usd)} of the ${usd(spend.cap_usd)} daily cap (${Math.round((spend.fraction ?? 0) * 100)}%). Cloud fallbacks stop at the cap.</div>`;
  }
  return "";
}

/** gateway_master_key — the LiteLLM master key: the API key local
 *  clients present and the admin UI password (username admin). The
 *  backend reads it from Windows Credential Manager (generating and
 *  storing on first use), so the returned value is the one in force. */
export async function getGatewayMasterKey(): Promise<CallResult<{ master_key: string }>> {
  if (!isTauri()) return { data: { master_key: "" }, live: false, error: "gateway_master_key is not available in sample mode" };
  try {
    const data = await invoke<{ master_key: string }>("gateway_master_key");
    return { data, live: true };
  } catch (e) {
    return { data: { master_key: "" }, live: true, error: errOf(e) };
  }
}

/** The app-managed PostgreSQL server as gateway_status reports it. */
export interface PostgresStatusData {
  installed: boolean;
  /** Lifecycle state: running, starting, stopping, stopped, failed,
   *  not_installed. */
  state: LifecycleState;
  pid: number | null;
  /** The configured port. */
  port: number | null;
  /** The running server listens on a different port than configured
   *  (postmaster.pid); the next gateway start restarts it. */
  port_mismatch: boolean;
  transition: TransitionWire | null;
}

/** The server-side activation apply in flight (gateway_status /
 *  activation plan `activation_applying`). */
export interface ActivationProgress {
  /** The surface that started it: desktop, api, cli. */
  source: string;
  mode: string;
  started_at: string;
  /** plan | backend | gateway */
  step: string;
  /** The backend id or "gateway" being restarted; null while reading the plan. */
  target: string | null;
  /** Operator-facing progress line ("Restarting backend planner…"). */
  message: string;
}

function numOrNull(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function strOr(v: unknown, d: string): string {
  return typeof v === "string" ? v : d;
}

/** Defensive parse of a `transition` object ({kind, origin, since}). */
export function parseTransition(v: unknown): TransitionWire | null {
  if (!v || typeof v !== "object") return null;
  const r = v as Record<string, unknown>;
  if (r.kind !== "starting" && r.kind !== "stopping" && r.kind !== "restarting") return null;
  return { kind: r.kind, origin: strOr(r.origin, ""), since: strOr(r.since, "") };
}

function parseFailure(v: unknown): FailureWire | null {
  if (!v || typeof v !== "object") return null;
  const r = v as Record<string, unknown>;
  if (typeof r.reason !== "string") return null;
  return { at: strOr(r.at, ""), reason: r.reason, exit_code: numOrNull(r.exit_code) };
}

function parsePostgres(v: unknown): PostgresStatusData | null {
  if (!v || typeof v !== "object") return null;
  const r = v as Record<string, unknown>;
  const installed = r.installed !== false;
  return {
    installed,
    state: r.state === undefined || r.state === null ? (installed ? "unknown" : "not_installed") : fromWire(r.state),
    pid: numOrNull(r.pid),
    port: numOrNull(r.port),
    port_mismatch: r.port_mismatch === true,
    transition: parseTransition(r.transition),
  };
}

/** Defensive parse of an `activation_applying` object; null when idle. */
export function parseActivationProgress(v: unknown): ActivationProgress | null {
  if (!v || typeof v !== "object") return null;
  const r = v as Record<string, unknown>;
  return {
    source: strOr(r.source, ""),
    mode: strOr(r.mode, ""),
    started_at: strOr(r.started_at, ""),
    step: strOr(r.step, ""),
    target: typeof r.target === "string" ? r.target : null,
    message: strOr(r.message, "Applying the activation plan…"),
  };
}

/** gateway_status — the gateway's lifecycle state (`state`, with the
 *  transition, intent, pid and failure behind it), the PostgreSQL
 *  sub-state, the activation apply in flight, the probe (`up`), port,
 *  group/rule counts and the recorded routing-test trace count. Every
 *  field is parsed defensively; a payload without `state` derives it
 *  from `up`. In sample mode: the mock status (gateway running). */
export async function getGatewayStatus(): Promise<CallResult<GatewayStatusData>> {
  const fallback: GatewayStatusData = {
    port: 4000, groups: 0, routing_rules: 0, up: false, state: "unknown", transition: null, intent: null, pid: null,
    service_managed: false, failure: null, postgres: null, activation_applying: null, routing_log_len: 0,
    openrouter_cred_present: null, bind_host: "0.0.0.0", lan_ip: null, activation_pending: false, activation_changes: 0,
    litellm_version: null,
  };
  if (!isTauri()) return { data: mockGatewayStatus(), live: false };
  try {
    const raw = await invoke<Record<string, unknown>>("gateway_status");
    const up = Boolean(raw.up);
    const state: LifecycleState = raw.state === undefined || raw.state === null
      ? (up ? "running" : "stopped")
      : fromWire(raw.state);
    return {
      data: {
        port: typeof raw.saved_port === "number" ? raw.saved_port : typeof raw.port === "number" ? raw.port : 0,
        groups: typeof raw.groups === "number" ? raw.groups : 0,
        routing_rules: typeof raw.routing_rules === "number" ? raw.routing_rules : 0,
        up,
        state,
        transition: parseTransition(raw.transition),
        intent: raw.intent === "running" || raw.intent === "stopped" ? raw.intent : null,
        pid: numOrNull(raw.pid),
        service_managed: raw.service_managed === true,
        failure: parseFailure(raw.failure),
        postgres: parsePostgres(raw.postgres),
        activation_applying: parseActivationProgress(raw.activation_applying),
        routing_log_len: typeof raw.routing_log_len === "number" ? raw.routing_log_len : 0,
        openrouter_cred_present: typeof raw.openrouter_cred_present === "boolean" ? raw.openrouter_cred_present : null,
        bind_host: typeof raw.bind_host === "string" ? raw.bind_host : "0.0.0.0",
        lan_ip: typeof raw.lan_ip === "string" ? raw.lan_ip : null,
        activation_pending: Boolean(raw.activation_pending),
        activation_changes: typeof raw.activation_changes === "number" ? raw.activation_changes : 0,
        litellm_version: typeof raw.litellm_version === "string" ? raw.litellm_version : null,
        openrouter_spend: parseOpenRouterSpend(raw.openrouter_spend),
        running_port: numOrNull(raw.running_port),
        port_pending: raw.port_pending === true,
        running_strategy: typeof raw.running_strategy === "string" ? raw.running_strategy : null,
        applied_strategies: parseAppliedStrategies(raw.applied_strategies ?? raw.group_strategies),
      },
      live: true,
    };
  } catch (e) {
    return { data: { ...fallback, port: 0 }, live: true, error: errOf(e) };
  }
}

/** gateway_set_bind_host — set the gateway bind address. Applies at
 *  the next gateway start (restart_required is always true); the
 *  start-gateway step reconciles the Windows firewall rule then. */
export async function setGatewayBindHost(host: string): Promise<CallResult<{ bind_host: string; restart_required: boolean }>> {
  if (!isTauri()) return { data: { bind_host: host, restart_required: true }, live: false };
  try {
    const data = await invoke<{ bind_host: string; restart_required: boolean }>("gateway_set_bind_host", { host });
    return { data, live: true };
  } catch (e) {
    return { data: { bind_host: host, restart_required: true }, live: true, error: errOf(e) };
  }
}

/** The gateway_stop result. `stopped` reports whether a gateway process
 *  was actually terminated (false with ok=true: it was not running);
 *  `postgres_stopped` whether PostgreSQL was stopped too (only when
 *  asked). */
export interface GatewayStopResult {
  ok: boolean;
  stopped: boolean;
  pid?: number | null;
  found_by?: string | null;
  postgres_stopped?: boolean;
}

/** gateway_stop — stop the running gateway (the counterpart of the
 *  wizard's start-gateway step). Stops the gateway ONLY: PostgreSQL
 *  keeps running unless `stopPostgres` is set (an explicit "stop the
 *  gateway and its database"). `origin` names who asked in the
 *  gateway's lifecycle log; the server defaults to ui. `busy` is set
 *  when another gateway operation is in flight. */
export async function gatewayStop(opts?: { stopPostgres?: boolean; origin?: string }): Promise<CallResult<GatewayStopResult> & { busy?: boolean }> {
  if (!isTauri()) return { data: { ok: false, stopped: false }, live: false, error: "gateway_stop is not available in sample mode" };
  try {
    const args: Record<string, unknown> = {};
    if (opts?.stopPostgres) args.stopPostgres = true;
    if (opts?.origin) args.origin = opts.origin;
    const data = await invoke<GatewayStopResult>("gateway_stop", args);
    return { data, live: true };
  } catch (e) {
    const error = errOf(e);
    return { data: { ok: false, stopped: false }, live: true, error, busy: isBusyError(error) };
  }
}

/** The gateway_restart result: the start-gateway step's outcome run
 *  under one "restarting" transition (the step stops the old gateway
 *  itself; PostgreSQL is never stopped). `ok` false with `busy` when
 *  another gateway operation (or a setup step) holds it. */
export interface GatewayRestartResult {
  ok: boolean;
  busy?: boolean;
  running_step?: string;
  elapsed_secs?: number;
  task_id?: string;
  outcome: WizardStepOutcome | null;
  message: string | null;
  manual_command: string | null;
  /** The drain result when `drain` was asked. */
  quiesce?: { drained: boolean; in_flight: number; timed_out: boolean };
}

/** gateway_restart — restart the gateway server-side (optionally
 *  draining in-flight requests first). Status surfaces read
 *  "restarting" for the whole operation. */
export async function gatewayRestart(opts?: { origin?: string; drain?: boolean }): Promise<CallResult<GatewayRestartResult>> {
  const empty: GatewayRestartResult = { ok: false, outcome: null, message: null, manual_command: null };
  if (!isTauri()) return { data: empty, live: false, error: "gateway_restart is not available in sample mode" };
  try {
    const args: Record<string, unknown> = {};
    if (opts?.origin) args.origin = opts.origin;
    if (opts?.drain !== undefined) args.drain = opts.drain;
    const raw = await invoke<Partial<GatewayRestartResult>>("gateway_restart", args);
    const outcome = raw.outcome ?? null;
    return {
      data: {
        ok: raw.ok === true,
        busy: raw.busy === true ? true : undefined,
        running_step: raw.running_step,
        elapsed_secs: raw.elapsed_secs,
        task_id: raw.task_id,
        outcome,
        message: typeof raw.message === "string" ? raw.message : outcome?.message ?? null,
        manual_command: typeof raw.manual_command === "string" ? raw.manual_command : outcome?.manual_command ?? null,
        quiesce: raw.quiesce,
      },
      live: true,
    };
  } catch (e) {
    const error = errOf(e);
    return { data: { ...empty, busy: isBusyError(error) || undefined, message: error }, live: true, error };
  }
}

/** postgres_stop — stop the app-managed PostgreSQL server. Refused
 *  while the gateway is running or in transition ("stop the gateway
 *  first"). */
export async function postgresStop(opts?: { origin?: string }): Promise<CallResult<{ ok: boolean; stopped: boolean }> & { busy?: boolean }> {
  if (!isTauri()) return { data: { ok: false, stopped: false }, live: false, error: "postgres_stop is not available in sample mode" };
  try {
    const args: Record<string, unknown> = {};
    if (opts?.origin) args.origin = opts.origin;
    const data = await invoke<{ ok: boolean; stopped: boolean }>("postgres_stop", args);
    return { data, live: true };
  } catch (e) {
    const error = errOf(e);
    return { data: { ok: false, stopped: false }, live: true, error, busy: isBusyError(error) };
  }
}

/** update_gateway_config — merge a JSON patch into the gateway
 *  configuration; `persisted` reports whether the change was saved to
 *  the configuration file (an in-memory-only apply reverts on restart). */
export async function updateGatewayConfig(patch: Record<string, unknown>): Promise<CallResult<{ ok: boolean; persisted: boolean }>> {
  if (!isTauri()) return { data: { ok: false, persisted: false }, live: false, error: "update_gateway_config is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean; persisted: boolean }>("update_gateway_config", { patch });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false, persisted: false }, live: true, error: errOf(e) };
  }
}

/** gateway_set_backend_group — move a backend to a group, or
 *  out of all groups when `group` is null (predicted-throughput
 *  design §8.5, decision D5: one group per backend). The move
 *  removes the id from every group's member list and appends it
 *  to the target's, journaled for the Activation Plan like
 *  every other membership change. `removed_from` names every
 *  group the backend left — more than one only when a
 *  hand-edited config had it in several. A drop on the lane the
 *  backend solely occupies succeeds with `changed: false` and
 *  no config write. */
export async function gatewaySetBackendGroup(id: string, group: string | null): Promise<CallResult<{ backend_id: string; group: string | null; removed_from: string[]; changed: boolean; persisted: boolean }>> {
  if (!isTauri()) return { data: { backend_id: id, group, removed_from: [], changed: false, persisted: false }, live: false, error: "gateway_set_backend_group is not available in sample mode" };
  try {
    const data = await invoke<{ backend_id: string; group: string | null; removed_from: string[]; changed: boolean; persisted: boolean }>("gateway_set_backend_group", { id, group });
    return { data, live: true };
  } catch (e) {
    return { data: { backend_id: id, group, removed_from: [], changed: false, persisted: false }, live: true, error: errOf(e) };
  }
}

/** gateway_create_group — create an empty group (design §8.5,
 *  decision D11). The name follows the group slug rule
 *  (lowercase letters, digits, hyphens). */
export async function gatewayCreateGroup(name: string): Promise<CallResult<{ name: string; persisted: boolean }>> {
  if (!isTauri()) return { data: { name, persisted: false }, live: false, error: "gateway_create_group is not available in sample mode" };
  try {
    const data = await invoke<{ name: string; persisted: boolean }>("gateway_create_group", { name });
    return { data, live: true };
  } catch (e) {
    return { data: { name, persisted: false }, live: true, error: errOf(e) };
  }
}

/** gateway_delete_group — delete an EMPTY group (design §8.5,
 *  decision D11); a group with members is refused with the
 *  member count, never cascaded. */
export async function gatewayDeleteGroup(name: string): Promise<CallResult<{ name: string; deleted: boolean; persisted: boolean }>> {
  if (!isTauri()) return { data: { name, deleted: false, persisted: false }, live: false, error: "gateway_delete_group is not available in sample mode" };
  try {
    const data = await invoke<{ name: string; deleted: boolean; persisted: boolean }>("gateway_delete_group", { name });
    return { data, live: true };
  } catch (e) {
    return { data: { name, deleted: false, persisted: false }, live: true, error: errOf(e) };
  }
}

/** gateway_preview_yaml — the generated LiteLLM config.yaml, plus
 *  the generated llmdil_routing.py callback when a typed routing
 *  rule is enabled (null otherwise). */
export async function getGatewayYaml(): Promise<CallResult<{ yaml: string; routing_py: string | null }>> {
  if (!isTauri()) return { data: { yaml: "", routing_py: null }, live: false };
  try {
    const data = await invoke<{ yaml: string; routing_py?: string | null }>("gateway_preview_yaml");
    return { data: { yaml: data.yaml, routing_py: data.routing_py ?? null }, live: true };
  } catch (e) {
    return { data: { yaml: "", routing_py: null }, live: true, error: errOf(e) };
  }
}

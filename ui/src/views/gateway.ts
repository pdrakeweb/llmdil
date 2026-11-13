/* Gateway view (split from static.ts). */

import { getActivationPlan, getBackendConfig, getBackends, getGatewayConfig, getGatewayMasterKey, getGatewayStatus, getGatewayYaml, getModels, openRouterSpendBannerHtml, openUrl, setGatewayBindHost, updateGatewayConfig } from "../api";
import type { ActivationPlanData, BackendRow, GatewayConfigData, GatewayGroup, GatewayRule, GatewayRuleKind, GatewayStatusData, ModelRow } from "../api";
import { fmtCount, meter, stackedBar, trim1 } from "../charts";
import { esc, setHtml } from "../html";
import { icon } from "../icons";
import { META, lifecycleChip, lifecycleDot, lifecycleLabel, renderLifecycleActions, serviceStore, wireLifecycleActions } from "../lifecycle";
import type { LifecycleResult, LifecycleState } from "../lifecycle";
import { enableSwitch } from "../switch";
import { activationPlanHtml, handlePlanAction, manualCommandHtml, newPlanUi } from "./activationPlan";
import type { PlanUiState } from "./activationPlan";
import { C, bannerHtml, emptyHtml, loadingHtml, sameModelFile, setBanner, topbar } from "./common";

/* ---------------- Gateway ---------------- */

/** Routing-rules card form state (routing-rules design §7), held
 *  in the wireGateway closure so the 5-second repaint preserves an
 *  open form. `editingId` null = adding a new rule; `optimalTokens`
 *  is the raw input text — empty means the fraction default. */
interface RuleFormState {
  open: boolean;
  editingId: string | null;
  target: string;
  optimalTokens: string;
  advisory: boolean;
}

/** The freshest gateway status: the shared store's snapshot when it
 *  has one (it polls fast during transitions and refreshes on the
 *  backend's lifecycle events), else the view's own load. */
function freshGatewayStatus(status: GatewayStatusData): GatewayStatusData {
  return serviceStore.get().gateway ?? status;
}

/** The gateway's effective lifecycle state and transition (the store's
 *  pending overlay applied: a Restart click reads "restarting…" at
 *  once, and a restart from any other surface reads the same). */
function gatewayLc(status: GatewayStatusData): { state: LifecycleState; transition: GatewayStatusData["transition"] } {
  const g = freshGatewayStatus(status);
  return {
    state: serviceStore.effective("gateway", g.state),
    transition: serviceStore.transitionOf("gateway", g.transition),
  };
}

/** The Gateway view's summary title: lifecycle dot + label (with the
 *  transition's origin and age), plus a "service" chip when the
 *  gateway runs as a Windows service. */
function gatewayLcTitleHtml(status: GatewayStatusData): string {
  const g = freshGatewayStatus(status);
  const { state, transition } = gatewayLc(status);
  const fail = state === "failed" && g.failure ? ` title="${esc(g.failure.reason)}"` : "";
  return `${lifecycleDot(state)} <span${fail}>Gateway \u2014 LiteLLM \u00b7 ${esc(lifecycleLabel(state, transition))}</span>`
    + (g.service_managed ? ` <span class="chip status-tag status-off" title="The gateway runs as a Windows service (SCM)">service</span>` : "")
    + (g.pid !== null && META[state].isLive ? ` <span style="font-size:11px;color:var(--text3);font-weight:400">pid ${g.pid}</span>` : "");
}

/** Start / Stop / Restart for the gateway, from the shared rule. */
function gatewayLcActionsHtml(status: GatewayStatusData): string {
  const { state } = gatewayLc(status);
  return renderLifecycleActions({ kind: "gateway" }, state, {}, {
    labels: { start: "Start gateway", stop: "Stop gateway", restart: "Restart gateway" },
  });
}

/** The app-managed PostgreSQL sub-row under the gateway title: its
 *  lifecycle chip, pid and port, a port-mismatch warning (the next
 *  gateway start restarts it on the configured port), and "Stop
 *  database" — enabled only while the gateway is stopped, since the
 *  gateway uses the database. PostgreSQL starts with the gateway. */
function gatewayPgRowHtml(status: GatewayStatusData): string {
  const pg = freshGatewayStatus(status).postgres;
  if (!pg) return "";
  if (!pg.installed && pg.state === "not_installed") {
    return `<div style="font-size:12px;color:var(--text2);margin:-4px 0 10px">PostgreSQL: not installed<span class="info-tip" title="The LiteLLM admin UI needs a database — run the setup wizard's Install PostgreSQL step.">${icon("info")}</span></div>`;
  }
  const ps = serviceStore.effective("postgres", pg.state);
  const pt = serviceStore.transitionOf("postgres", pg.transition);
  const gs = gatewayLc(status).state;
  const gatewayDown = gs === "stopped" || gs === "failed" || gs === "unknown" || gs === "not_installed";
  const parts = [
    pg.pid !== null && META[ps].isLive ? `pid ${pg.pid}` : "",
    pg.port !== null ? `port ${pg.port}` : "",
  ].filter(Boolean).join(" \u00b7 ");
  const mismatch = pg.port_mismatch
    ? ` <span style="color:var(--warn)" title="The running server listens on a different port than configured. The next gateway start restarts PostgreSQL on the configured port.">${icon("warn")} port differs from the configuration</span>`
    : "";
  const stopBtn = renderLifecycleActions({ kind: "postgres" }, ps, {
    blockedReason: gatewayDown ? undefined : "Stop the gateway first — it uses the database",
  }, { className: "btn", labels: { stop: "Stop database" } });
  return `<div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap;font-size:12px;color:var(--text2);margin:-4px 0 10px">PostgreSQL ${lifecycleChip(ps, { transition: pt })}${parts ? ` <span>${esc(parts)}</span>` : ""}${mismatch}<span class="spacer" style="flex:1"></span>${stopBtn}</div>`;
}

function gatewayPaint(
  status: GatewayStatusData,
  cfg: GatewayConfigData,
  backends: BackendRow[],
  models: ModelRow[],
  yamlOpen: boolean,
  yamlText: string | null,
  yamlErr: string,
  backendsError: string | null,
  masterKey: string | null,
  keyRevealed: boolean,
  plan: ActivationPlanData | null,
  planUi: PlanUiState,
  routingTab: "flow" | "groups",
  orModelsOpen: boolean,
  openFragments: Set<string>,
  ruleForm: RuleFormState,
  confirmRuleDelete: Set<string>,
  ruleRawFlags: Map<string, string | null>,
  genTab: "yaml" | "routing",
  routingPyText: string | null,
): string {
  const port = status.port || cfg.port;
  const backendById = new Map(backends.map((b) => [b.id, b]));
  /** Is this member dropped at generation time, and why? A backend
   *  member is dropped when disabled; the openrouter member when its
   *  tier is disabled. A member naming neither (an unknown name)
   *  cannot be verified as excluded and is treated as active.
   *  Exclusion is a settled state, so it renders gray (principle
   *  11) with the reason in a tooltip — never amber. */
  const memberState = (name: string): { excluded: boolean; reason: string } => {
    if (name === "openrouter") {
      return cfg.openrouter && !cfg.openrouter.enabled
        ? { excluded: true, reason: "tier disabled" }
        : { excluded: false, reason: "" };
    }
    const b = backendById.get(name);
    return b && !b.enabled
      ? { excluded: true, reason: "backend disabled" }
      : { excluded: false, reason: "" };
  };
  const memberExcluded = (name: string): boolean => memberState(name).excluded;
  const groupByName = new Map(cfg.groups.map((g) => [g.name, g]));
  /** A fallback naming a group whose members are ALL excluded (or
   *  which has no members) produces no deployments — the generated
   *  config drops the fallback reference itself. */
  const fallbackState = (name: string): { excluded: boolean; reason: string } => {
    const g = groupByName.get(name);
    if (g && name !== "openrouter") {
      return g.members.length === 0 || g.members.every(memberExcluded)
        ? { excluded: true, reason: "no active members" }
        : { excluded: false, reason: "" };
    }
    return memberState(name);
  };

  // The exclusion chips above are computed from the backend list; when
  // that list failed to load, say so instead of silently rendering
  // members as included.
  const backendsNote = backendsError
    ? `<div class="banner" style="margin-bottom:12px">${icon("warn")} Backend list unavailable (${esc(backendsError)}) \u2014 exclusion states in this view may be incomplete.</div>`
    : "";

  const withWin = backends.filter((b) => b.requests_1h !== undefined);
  // The traffic block renders only when at least one backend has a
  // metrics window (no placeholder line otherwise). Requests are a
  // per-backend distribution, so they render as a stacked bar whose
  // segments are the reporting backends (the largest segment IS the
  // hottest backend — no separate tile); the totals live once, as
  // the rows' Label: Value. Totals sum reporting backends only, and
  // partial coverage is stated in the Requests row's info-tip —
  // never claimed as "all backends" (principle 12). Errors render
  // as their share of requests in a thin proportion bar.
  const gwStats = withWin.length
    ? (() => {
        const totalReq = withWin.reduce((a, b) => a + (b.requests_1h ?? 0), 0);
        const totalErr = withWin.reduce((a, b) => a + (b.errors_1h ?? 0), 0);
        // Throughput sums only backends whose live rate is
        // measured; the row is omitted when none is (unknown is
        // not zero).
        const tputRows = withWin.filter((b) => b.throughput_toks !== undefined);
        const totalToks = tputRows.length
          ? tputRows.reduce((a, b) => a + (b.throughput_toks ?? 0), 0)
          : undefined;
        const palette = [C.blue, C.purple, C.teal, C.amber, C.green];
        const segs = withWin.map((b, i) => ({
          pct: totalReq > 0 ? ((b.requests_1h ?? 0) / totalReq) * 100 : 0,
          color: palette[i % palette.length],
          label: `${b.id} — ${fmtCount(b.requests_1h ?? 0)} req (1h)`,
        }));
        const keys = withWin.map((b, i) => `<span><span class="sw" style="background:${palette[i % palette.length]}"></span>${esc(b.id)} ${fmtCount(b.requests_1h ?? 0)}</span>`).join("");
        const coverageTip = withWin.length < backends.length
          ? `<span class="info-tip" title="${withWin.length} of ${backends.length} backends reporting — totals cover reporting backends only">${icon("info")}</span>`
          : "";
        const errSegs = totalReq > 0 && totalErr > 0
          ? [{ pct: (totalErr / totalReq) * 100, color: C.red, label: `Errors: ${fmtCount(totalErr)} of ${fmtCount(totalReq)} requests (1h)` }]
          : [];
        return `<div style="margin-top:12px;border-top:1px solid var(--border);padding-top:10px">
        <div style="font-size:12px;color:var(--text2)">Requests (1h): <b style="color:var(--text)">${fmtCount(totalReq)}</b>${coverageTip}</div>
        <div style="margin-top:4px">${stackedBar(segs, 600, 10)}</div>
        <div class="legend" style="margin-top:4px">${keys}</div>
        <div style="font-size:12px;color:var(--text2);margin-top:8px">Errors (1h): <b style="color:var(--text)">${fmtCount(totalErr)}</b></div>
        <div style="margin-top:4px">${stackedBar(errSegs, 600, 4)}</div>
        ${totalToks !== undefined ? `<div class="kv" style="margin-top:8px"><span class="k">Throughput</span><span class="v" style="font-size:13px">${trim1(totalToks)} tok/s</span></div>` : ""}
      </div>`;
      })()
    : "";
  const summary = `<div class="card" style="margin-bottom:12px"><div class="sec-title" id="gwLcTitle">${gatewayLcTitleHtml(status)}</div>
    <div id="gwPgRow">${gatewayPgRowHtml(status)}</div>
    <div style="display:grid;grid-template-columns:repeat(5,1fr);gap:10px">
      ${([["Port", String(port)], ["Groups", fmtCount(status.groups), "Enabled groups — a group counts when at least one of its member backends is enabled; the gateway serves enabled groups only."], ["Routing rules", fmtCount(status.routing_rules)], ["Routing test traces", fmtCount(status.routing_log_len), "Routing test traces are dry-run decisions recorded from routing tests this session — live requests are not logged."], ["LiteLLM", status.litellm_version ?? "not measured", "The LiteLLM version the start-gateway step probed in the gateway venv at the last spawn (MCP design §4.3). Not measured until a spawn records one."]] as [string, string, string?][])

        .map(([k, v, tip]) => `<div class="kv"><span class="k">${k}${tip ? `<span class="info-tip" title="${esc(tip)}">${icon("info")}</span>` : ""}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
    </div>
    ${gwStats}</div>`;

  // Bind explanations ride an info-tip on the Network access row —
  // the LAN row itself renders only when a LAN URL exists.
  const bindTip = status.bind_host === "127.0.0.1"
    ? "Reachable from this machine only."
    : status.lan_ip
      ? null
      : "Bound to all interfaces; this machine's LAN address could not be determined.";
  const masterKeyTip = `Clients authenticate with the master key — LiteLLM requires one. The VS Code LiteLLM extension takes the base URL without /v1 (http://127.0.0.1:${port}); it appends /v1 itself.${masterKey === null ? " Reveal loads it." : ""}`;
  // The gateway lifecycle controls answer "how do I control the
  // gateway", not the endpoint card's question ("what do clients
  // connect to") — the row renders directly under the summary card
  // instead (client-endpoint audit Q7). Ids and handlers unchanged.
  const gwActions = `<div class="card-actions" style="margin:0 0 12px">
      <span id="gwLcActions" style="display:contents">${gatewayLcActionsHtml(status)}</span><span class="info-tip" title="Start runs the Setup Wizard's start-gateway step: LiteLLM binds the configured address, and the step reconciles the Windows firewall rule with it (one elevation prompt when the rule state changes). When the OpenRouter tier is enabled, the fallback is armed from Windows Credential Manager. Restart replaces the running gateway in one server-side operation (PostgreSQL keeps running unless its port changed); Stop stops the gateway only.">${icon("info")}</span>
      <a class="btn" href="#/wizard">${icon("wrench")} Setup Wizard</a>
      <button class="btn icon-only" id="gwAdminUiBtn" title="LiteLLM admin UI" aria-label="LiteLLM admin UI">${icon("gateway")}</button><span class="info-tip" title="Sign in as admin with the master key as the password. The admin UI requires a database — run the setup wizard's Install PostgreSQL step if sign-in reports &quot;Not connected to DB&quot;.">${icon("info")}</span>
      <a class="btn icon-only" href="#/logs?scope=gateway&follow=1" title="Logs" aria-label="Gateway logs">${icon("logs")}</a>
    </div>`;
  const endpoint = `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gateway")} Client endpoint</div>
    <div style="font-size:12px;color:var(--text2);display:grid;gap:4px">
      <div>Base URL: <span style="font-family:var(--mono);color:var(--text)">http://127.0.0.1:${port}/v1</span>
        <button class="btn icon-only" id="gwBaseUrlCopyBtn" title="Copy base URL" aria-label="Copy base URL">${icon("copy")}</button></div>
      <div>Port:<span class="info-tip" title="The gateway's listen port, saved to the configuration through update_gateway_config and validated against every other claimed port. Applies when the gateway next starts; the activation plan tracks the restart.">${icon("info")}</span> <input id="gwPortInput" type="number" min="1" max="65535" value="${port}" style="width:90px;font-size:12px">
        <button class="btn" id="gwPortApply" style="padding:1px 7px;font-size:11px">Apply</button></div>
      <div>Network access:${bindTip ? `<span class="info-tip" title="${esc(bindTip)}">${icon("info")}</span>` : ""} <select id="gwBindSelect" class="input" style="width:auto;display:inline-block;padding:2px 6px">
          <option value="0.0.0.0"${status.bind_host !== "127.0.0.1" ? " selected" : ""}>Local network (all interfaces)</option>
          <option value="127.0.0.1"${status.bind_host === "127.0.0.1" ? " selected" : ""}>This computer only (127.0.0.1)</option>
        </select></div>
      ${status.bind_host !== "127.0.0.1" && status.lan_ip
        ? `<div>LAN base URL: <span style="font-family:var(--mono);color:var(--text)">http://${esc(status.lan_ip)}:${port}/v1</span><span class="info-tip" title="Other machines use this address with the master key. The VS Code LiteLLM extension takes it without /v1.">${icon("info")}</span>
        <button class="btn icon-only" id="gwLanUrlCopyBtn" title="Copy LAN base URL" aria-label="Copy LAN base URL">${icon("copy")}</button></div>`
        : ""}
      <div>Master key:<span class="info-tip" title="${esc(masterKeyTip)}">${icon("info")}</span> <span style="font-family:var(--mono);color:var(--text)">${masterKey === null ? "not loaded" : keyRevealed ? esc(masterKey) : "\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022"}</span>
        <button class="btn icon-only" id="gwKeyRevealBtn" title="${keyRevealed ? "Hide master key" : "Reveal master key"}" aria-label="${keyRevealed ? "Hide master key" : "Reveal master key"}">${icon(keyRevealed ? "eyeOff" : "eye")}</button>
        <button class="btn icon-only" id="gwKeyCopyBtn" title="Copy master key" aria-label="Copy master key">${icon("copy")}</button></div>
    </div></div>`;

  const or = cfg.openrouter;
  // The tier card is omitted entirely when the tier is not
  // configured (principle 13). State renders as header chips, the
  // credential's probed presence as a chip beside its label
  // (present / not found / not checked — the probe's null is an
  // explicit unknown, never prose), and every explanation rides a
  // tooltip verbatim. The reference is an address (of the secret in
  // Windows Credential Manager), so it carries a Copy button.
  const openrouter = or === null ? "" : (() => {
    const present = status.openrouter_cred_present;
    const credChip = or.cred_ref
      ? (present === true
        ? `<span class="chip status-tag status-on">present</span>`
        : present === false
          ? `<span class="chip status-tag status-bad" title="The fallback cannot authenticate.">not found</span>`
          : `<span class="chip">not checked</span>`)
      : or.enabled
        ? `<span class="chip status-tag status-bad" title="The fallback cannot authenticate until one is configured.">no credential reference</span>`
        : "";
    const credStatusRow = or.cred_ref || or.enabled
      ? `<div style="display:flex;justify-content:space-between;gap:16px"><span class="k">Credential status</span><span>${credChip}</span></div>`
      : "";
    const allowRow = `<div style="display:flex;justify-content:space-between;gap:16px"><span class="k">Model allowlist</span><span class="v" style="font-size:12px;text-align:right">${or.model_allowlist.length ? `${or.model_allowlist.length} model${or.model_allowlist.length === 1 ? "" : "s"}` : "none"} <button class="btn" data-or-models aria-expanded="${orModelsOpen}" title="Model allowlist" aria-label="Model allowlist" style="padding:1px 7px;font-size:11px">${icon(orModelsOpen ? "chevD" : "chevR")}</button></span></div>
      ${orModelsOpen ? `${or.model_allowlist.length ? `<div style="display:flex;gap:6px;flex-wrap:wrap;margin-top:4px">${or.model_allowlist.map((m) => `<span class="chip">${esc(m)}</span>`).join("")}</div>` : ""}
      <div style="margin-top:6px"><label style="font-size:12px;color:var(--text2)">Models, one per line
        <textarea id="gwOrAllowInput" rows="3" style="display:block;width:100%;font-size:12px;margin-top:4px">${esc(or.model_allowlist.join("\n"))}</textarea></label>
      <div class="card-actions" style="margin-top:6px"><button class="btn" id="gwOrAllowSave" style="padding:1px 7px;font-size:11px">Save allowlist</button></div></div>` : ""}`;
    // Spent-vs-cap (openrouter-tier audit's designated slot, now that
    // gateway_status carries the spend ledger): "Spent today: $X"
    // with a thin meter whose fill is spent / cap — the cap itself
    // lives once, in the Daily cap field above. Light red once the
    // cap is reached (no further OpenRouter traffic). Omitted when
    // the payload carries no spend or the cap is zero (no share).
    const spend = status.openrouter_spend ?? null;
    const spendRow = spend && spend.fraction !== null && spend.cap_usd > 0
      ? `<div data-or-spend><div style="display:flex;justify-content:space-between;gap:16px"><span class="k">Spent today<span class="info-tip" title="${esc(`The app's spend ledger for OpenRouter traffic today; the bar fills toward the daily cap ($${trim1(spend.cap_usd)}). The cap itself is enforced inside the gateway (LiteLLM provider budget, 1-day window).`)}">${icon("info")}</span></span><span class="v" style="font-size:12px">$${trim1(spend.spent_today_usd)}${spend.capped ? ` <span class="chip status-tag status-bad" title="The daily cap is reached: no OpenRouter traffic is sent until the window resets.">cap reached</span>` : ""}</span></div>
      <div style="margin-top:4px" title="${esc(`Spent: ${trim1(Math.min(1, spend.fraction) * 100)}% of the daily cap`)}">${meter(Math.min(100, spend.fraction * 100), 300, 6, spend.capped ? C.red : C.blue)}</div></div>`
      : "";
    return `<div class="card" style="margin-bottom:12px${or.enabled ? "" : ";opacity:.65"}"><div class="sec-title">${icon("gateway")} OpenRouter fallback tier
      <span class="spacer" style="flex:1"></span>
      ${or.enabled
        ? `<span class="chip status-tag status-on">enabled</span>`
        : `<span class="chip status-tag status-off" title="The tier is disabled — fallback entries naming openrouter are dropped when the gateway config is generated, and no traffic is sent to OpenRouter.">disabled</span>`}
    </div>
    <div class="kv" style="gap:10px">
      <div style="display:flex;justify-content:space-between;gap:16px;align-items:center"><span class="k">Credential<span class="info-tip" title="The Credential Manager target holding the OpenRouter key. When the gateway starts, the key is read from Windows Credential Manager (this reference) and passed to LiteLLM as OPENROUTER_API_KEY. The key itself is never stored in the configuration. Clear the field and save to remove the reference.">${icon("info")}</span></span><span style="display:flex;gap:6px;align-items:center"><input id="gwOrCredInput" type="text" value="${esc(or.cred_ref ?? "")}" placeholder="Credential Manager target" style="width:220px;font-size:12px"><button class="btn" id="gwOrCredSave" style="padding:1px 7px;font-size:11px">Save</button>${or.cred_ref ? `<button class="btn" id="gwOrCredCopyBtn" title="Copy credential reference" aria-label="Copy credential reference" style="padding:1px 7px;font-size:11px">${icon("copy")}</button>` : ""}</span></div>
      ${credStatusRow}
      <div style="display:flex;justify-content:space-between;gap:16px;align-items:center"><span class="k">Daily cap<span class="info-tip" title="Spending cap per day.">${icon("info")}</span></span><span style="display:flex;gap:6px;align-items:center"><span style="font-size:12px">$</span><input id="gwOrCapInput" type="number" min="0" step="0.5" value="${or.daily_cap_usd}" style="width:90px;font-size:12px"><button class="btn" id="gwOrCapSave" style="padding:1px 7px;font-size:11px">Save</button></span></div>
      ${spendRow}
      ${allowRow}
    </div>
    <div class="card-actions">${enableSwitch({ on: or.enabled, name: "OpenRouter fallback tier", attrs: `id="gwOrToggle"` })}</div>
  </div>`;
  })();

  // Computed by the backend (get_gateway_config.effective_strategy)
  // with the config generator's own rule; never re-derived here.
  const effStrategy = cfg.effective_strategy;

  /* ---- Routing section: one section, two views (Flow | Groups).
   * The section top carries the one answer-level fact — the
   * effective global strategy as Label: Value, its methodology in
   * the label's info-tip. ---- */
  const flowGroupHtml = (g: GatewayGroup): string => {
    // Member traffic shares: real only when every member is a
    // backend with a metrics window and the group total is
    // positive. Otherwise the bar encodes configured membership at
    // equal widths, and its info-tip states that basis — equal
    // width never invents traffic.
    const reqs = g.members.map((name) => backendById.get(name)?.requests_1h);
    const totalReq = reqs.reduce<number>((a, r) => a + (r ?? 0), 0);
    const traffic = g.members.length > 0 && reqs.every((r) => r !== undefined) && totalReq > 0;
    const palette = [C.blue, C.purple, C.teal, C.amber, C.green];
    const segs = g.members.map((name, i) => ({
      pct: traffic ? ((reqs[i] ?? 0) / totalReq) * 100 : 100 / g.members.length,
      color: traffic ? palette[i % palette.length] : "#3a4350",
      label: traffic ? `${name} — ${fmtCount(reqs[i] ?? 0)} req (1h)` : name,
    }));
    const keys = g.members.map((name) => {
      const st = memberState(name);
      return st.excluded
        ? `<span style="color:var(--text3)">${esc(name)} <span class="chip status-tag status-off" title="${esc(st.reason)}">excluded</span></span>`
        : `<span>${esc(name)}</span>`;
    }).join(" · ");
    const fallbackLines = g.fallbacks.map((name) => {
      const st = fallbackState(name);
      // An excluded fallback target gets NO connector — a connector
      // must never assert a path the generated config drops.
      return st.excluded
        ? `<div style="font-size:12px;margin-top:2px"><span class="chip status-tag status-off" title="${esc(st.reason)}">${esc(name)}</span></div>`
        : `<div style="font-size:12px;margin-top:2px"><span style="color:var(--text3)">fallback - - \u2192</span> ${esc(name)}</div>`;
    }).join("");
    const sub = g.members.length
      ? `Members enabled: ${g.enabled_members} of ${g.members.length}`
      : `<span title="Omitted from the generated gateway config.">Members: 0</span>`;
    return `<div style="display:flex;gap:10px;align-items:flex-start;padding:8px 0;border-bottom:1px solid var(--border)${g.enabled ? "" : ";opacity:.65"}">
      <div style="flex:0 0 170px;border:1px solid var(--border);border-radius:8px;padding:8px 10px;background:var(--panel2)">
        <div style="font-size:12px;font-weight:600">${esc(g.name)}${g.enabled ? "" : ` <span class="chip status-tag status-off">disabled</span>`}</div>
        <div style="font-size:11px;color:var(--text2);margin-top:2px">${sub}</div>
      </div>
      <span style="color:var(--text3);padding-top:10px">\u2192</span>
      <div style="flex:1;min-width:0">
        ${g.members.length ? `${stackedBar(segs, 600, 10)}${traffic ? "" : `<span class="info-tip" title="Configured membership — no metrics window yet.">${icon("info")}</span>`}
        <div style="font-size:12px;margin-top:4px">${keys}</div>` : ""}
        ${fallbackLines}
      </div>
    </div>`;
  };

  const groupCardHtml = (g: GatewayGroup): string => {
    const m = g.members.length;
    const enabledCount = g.members.filter((name) => !memberExcluded(name)).length;
    // One segment per member, equal width — segments encode
    // membership and state, not a share. Green reaches the
    // generated config; gray is excluded. The fraction lives only
    // in the bar's tooltip; member names are the bar's keys, each
    // in its segment's color.
    const bar = m
      ? `<div style="margin-top:10px"><span title="Enabled: ${enabledCount} of ${m}" style="display:block">${stackedBar(g.members.map((name) => ({
          pct: 100 / m,
          color: memberExcluded(name) ? "#6d7686" : C.green,
          label: name,
        })), 300, 10)}</span>
        <div style="font-size:12px;margin-top:4px;display:flex;gap:10px;flex-wrap:wrap">${g.members.map((name) => {
          const ex = memberExcluded(name);
          return `<span style="color:${ex ? "var(--text3)" : C.green}"${ex ? ` title="Disabled — dropped from the generated gateway config"` : ""}>${esc(name)}</span>`;
        }).join("")}</div></div>`
      : "";
    const fallbacks = g.fallbacks.length
      ? `<div style="font-size:12px;margin-top:8px"><span style="color:var(--text2)">Fallbacks:</span> ${g.fallbacks.map((name) => {
          const st = fallbackState(name);
          const tip = st.reason === "no active members"
            ? "No active members — dropped from the generated gateway config"
            : "Disabled — dropped from the generated gateway config";
          return `<span style="color:${st.excluded ? "var(--text3)" : C.green}"${st.excluded ? ` title="${tip}"` : ""}>${esc(name)}</span>`;
        }).join(" · ")}</div>`
      : "";
    return `<div class="card"${g.enabled ? "" : ` style="opacity:.65"`}>
      <h3>${esc(g.name)}${g.enabled ? "" : ` <span class="chip status-tag status-off" title="No enabled backends — omitted from the generated gateway config">disabled</span>`}
        <span class="spacer" style="flex:1"></span><span class="chip">${esc(g.strategy)}</span></h3>
      ${bar}
      ${fallbacks}
      <div style="display:flex;gap:6px;align-items:center;margin-top:8px;flex-wrap:wrap">
        <label style="font-size:12px;color:var(--text2)">Fallbacks <input type="text" data-group-fallbacks-input="${esc(g.name)}" value="${esc(g.fallbacks.join(", "))}" style="width:240px;font-size:12px"></label>
        <button class="btn" data-group-fallbacks-save="${esc(g.name)}" style="padding:1px 7px;font-size:11px">Save</button>
        <span class="info-tip" title="The ordered fallback chain for this group: backend ids, or openrouter for the cloud tier, comma-separated. Saved through update_gateway_config and validated — an unknown name is refused. Applies when the gateway next starts.">${icon("info")}</span>
      </div>
    </div>`;
  };

  const routing = cfg.groups.length
    ? `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gateway")} Routing<span class="spacer" style="flex:1"></span><a class="btn" href="#/groups">${icon("pencil")} Edit membership</a></div>
      ${effStrategy ? `<div style="font-size:12px;color:var(--text2)">Effective strategy: <b style="color:var(--text)">${esc(effStrategy)}</b><span class="info-tip" title="LiteLLM applies ONE global routing strategy — the strategy chips on the groups are labels, not per-group settings.">${icon("info")}</span></div>` : ""}
      <div class="viewsel" role="tablist" style="margin-top:10px">
        <button data-routing-tab="flow"${routingTab === "flow" ? ` class="active"` : ""}>Flow</button>
        <button data-routing-tab="groups"${routingTab === "groups" ? ` class="active"` : ""}>Groups</button>
      </div>
      ${routingTab === "flow"
        ? `<div>${cfg.groups.map(flowGroupHtml).join("")}</div>`
        : `<div class="grid2" style="margin-top:12px">${cfg.groups.map(groupCardHtml).join("")}</div>`}
    </div>`
    : emptyHtml("No model groups are configured for the gateway.");

  /* ---- Routing rules (routing-rules design §7): the management
   * surface for typed rules. The header renders always — it is the
   * creation surface, not a data block; the body renders when rules
   * exist or the form is open. Typed rules render as Label: Value
   * rows with enable/disable (icon-only), edit (icon-only), and
   * Delete (words + icon, destructive — principle 15); legacy
   * fragment-only rules stay read-only (design §6.1) with their
   * fragment behind a per-row expandable. The source composition
   * bar survives from the read-only card for mixed rule lists. ---- */
  /** A group's hard window, mirroring manager-gateway's baking:
   *  the minimum over its enabled member backends of
   *  min(configured context, the model's rated context). Null when
   *  no enabled backend member bounds the group. */
  const groupHardWindow = (g: GatewayGroup): number | null => {
    let hard: number | null = null;
    for (const member of g.members) {
      const b = backendById.get(member);
      if (!b || !b.enabled || b.n_ctx === undefined) continue;
      const mdl = models.find((m) => sameModelFile(b.model_file, m.gguf_path));
      const cap = mdl?.ctx_max ? mdl.ctx_max : null;
      const w = cap !== null ? Math.min(b.n_ctx, cap) : b.n_ctx;
      hard = hard === null ? w : Math.min(hard, w);
    }
    return hard;
  };
  /** The §6.2 warnings for a rule draft targeting `target`,
   *  computed from the same data the generator bakes from. */
  const ruleWarnings = (target: string): string[] => {
    const warnings: string[] = [];
    const tg = cfg.groups.find((g) => g.name === target);
    if (!tg) return warnings;
    if (tg.enabled_members === 0) {
      warnings.push(`The target group ${target} has no enabled members — rerouted requests will fail until a member backend is enabled.`);
    }
    const targetHard = groupHardWindow(tg);
    if (targetHard !== null) {
      for (const g of cfg.groups) {
        if (g.name === target) continue;
        const h = groupHardWindow(g);
        if (h !== null && targetHard <= h) {
          warnings.push(`The target's window (${fmtCount(targetHard)} tokens) is not larger than ${g.name}'s (${fmtCount(h)} tokens) — the rule cannot help ${g.name}.`);
        }
      }
    }
    for (const g of cfg.groups) {
      for (const member of g.members) {
        const raw = ruleRawFlags.get(member);
        if (!raw) continue;
        const toks = raw.split(/\s+/);
        for (let i = 0; i + 1 < toks.length; i++) {
          if ((toks[i] === "--parallel" || toks[i] === "-np") && Number(toks[i + 1]) > 1) {
            warnings.push(`${member} runs with ${toks[i]} ${toks[i + 1]} — its per-slot context is smaller than the window the rule assumes, so rerouting can trigger later than the rule intends.`);
          }
        }
      }
    }
    return warnings;
  };
  const ruleOptimalText = (r: GatewayRule): string => {
    const k = r.kind as GatewayRuleKind;
    if (k.optimal_tokens !== null) return `${fmtCount(k.optimal_tokens)} tokens`;
    const tg = cfg.groups.find((g) => g.name === k.target_group);
    const hard = tg ? groupHardWindow(tg) : null;
    const pct = `${Math.round(k.optimal_fraction * 100)}%`;
    return hard !== null
      ? `${fmtCount(Math.floor(k.optimal_fraction * hard))} tokens (${pct} of the target window)`
      : `${pct} of the serving window`;
  };
  const typedRuleRow = (r: GatewayRule): string => {
    const k = r.kind as GatewayRuleKind;
    return `<div style="display:flex;gap:14px;align-items:center;padding:6px 0;border-bottom:1px solid var(--border);flex-wrap:wrap">
      <span style="font-size:12px">Type: <b>Context overflow</b></span>
      <span style="font-size:12px">Target: <b>${esc(k.target_group)}</b></span>
      <span style="font-size:12px">Optimal: <b>${esc(ruleOptimalText(r))}</b></span>
      <span style="font-size:12px">Advisory: <b>${k.advisory ? "On" : "Off"}</b></span>
      ${r.description ? `<span class="info-tip" title="${esc(r.description)}">${icon("info")}</span>` : ""}
      ${r.enabled ? "" : `<span class="chip status-tag status-off">Disabled</span>`}
      <span class="spacer" style="flex:1"></span>
      ${enableSwitch({ on: r.enabled, name: `Routing rule ${r.id}`, attrs: `data-rule-toggle="${esc(r.id)}"` })}
      <button class="btn icon-only" data-rule-edit="${esc(r.id)}" title="Edit rule" aria-label="Edit rule ${esc(r.id)}">${icon("pencil")}</button>
      <button class="btn danger${confirmRuleDelete.has(r.id) ? " armed" : ""}" data-rule-delete="${esc(r.id)}">${icon("trash")} ${confirmRuleDelete.has(r.id) ? "Confirm delete" : "Delete"}</button>
    </div>`;
  };
  const legacyRuleRow = (r: GatewayRule): string => {
    const open = openFragments.has(r.id);
    return `<div style="display:flex;gap:8px;align-items:center;padding:6px 0;border-bottom:1px solid var(--border)">
      <span style="font-size:12px">Rule: <span style="font-family:var(--mono)">${esc(r.id)}</span></span>
      <span class="info-tip" title="${esc(r.description)}">${icon("info")}</span>
      <span class="spacer" style="flex:1"></span>
      ${r.litellm_fragment ? `<button class="btn" data-fragment="${esc(r.id)}" aria-expanded="${open}" title="Fragment" aria-label="Fragment" style="padding:2px 8px;font-size:11px">${icon(open ? "chevD" : "chevR")}</button>` : ""}
      <span class="chip">${esc(r.source)}</span></div>
      ${open && r.litellm_fragment ? `<pre style="margin:6px 0;padding:10px 12px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-word">${esc(r.litellm_fragment)}</pre>` : ""}`;
  };
  const ruleFormHtml = (): string => {
    if (!ruleForm.open) return "";
    const warnings = ruleWarnings(ruleForm.target);
    const targetOptions = cfg.groups
      .map((g) => `<option value="${esc(g.name)}"${g.name === ruleForm.target ? " selected" : ""}>${esc(g.name)}</option>`)
      .join("");
    return `<div style="margin-top:10px;border-top:1px solid var(--border);padding-top:10px">
      <div style="display:flex;gap:16px;align-items:flex-end;flex-wrap:wrap">
        <span style="font-size:12px">Type: <b>Context overflow</b></span>
        <label style="font-size:12px">Target group
          <select id="gwRuleTarget" class="input" style="display:block;margin-top:4px">${targetOptions}</select></label>
        <label style="font-size:12px">Optimal tokens<span class="info-tip" title="Past this conversation size the serving model adds the compaction note. Empty = the default: half the serving model's window.">${icon("info")}</span>
          <input id="gwRuleOptimal" class="input" type="number" min="1" step="1" placeholder="Default (half the window)" value="${esc(ruleForm.optimalTokens)}" style="display:block;margin-top:4px"></label>
        <label style="font-size:12px;display:flex;gap:6px;align-items:center;padding-bottom:6px"><input type="checkbox" id="gwRuleAdvisory"${ruleForm.advisory ? " checked" : ""}> Advisory<span class="info-tip" title="When on, the serving model appends the compaction note past the optimal window. Off = reroute silently.">${icon("info")}</span></label>
      </div>
      ${warnings.map((w) => `<div style="color:var(--amber);font-size:12px;margin-top:6px">${icon("warn")} ${esc(w)}</div>`).join("")}
      <div class="card-actions" style="margin-top:10px">
        <button class="btn" id="gwRuleSaveBtn">${icon("check")} Save rule</button>
        <button class="btn" id="gwRuleCancelBtn">Cancel</button>
      </div>
    </div>`;
  };
  const rules = (() => {
    const bySource = new Map<string, number>();
    for (const r of cfg.routing_rules) bySource.set(r.source, (bySource.get(r.source) ?? 0) + 1);
    const tones = ["#6d7686", "#3a4350", "#a7b0bf", "#545e6e"];
    const showBar = cfg.routing_rules.length >= 2 && bySource.size >= 2;
    const bar = showBar
      ? `<div style="margin-bottom:6px">${stackedBar([...bySource.entries()].map(([src, cnt], i) => ({
          pct: (cnt / cfg.routing_rules.length) * 100,
          color: tones[i % tones.length],
          label: `${src}: ${cnt}`,
        })), 600, 8)}
        <div class="legend" style="margin-top:4px">${[...bySource.entries()].map(([src], i) => `<span><span class="sw" style="background:${tones[i % tones.length]}"></span>${esc(src)}</span>`).join("")}</div></div>`
      : "";
    const rows = cfg.routing_rules.map((r) => (r.kind ? typedRuleRow(r) : legacyRuleRow(r))).join("");
    return `<div class="card" style="margin-top:12px"><div class="sec-title">${icon("gateway")} Routing rules<span class="info-tip" title="A context-overflow rule reroutes a request too large for its model group to the target group instead of failing it, and past the optimal window the serving model adds a one-line note advising a compacted or fresh conversation. Changes take effect when the gateway restarts — save, then apply the Activation Plan.">${icon("info")}</span>
      <span class="spacer" style="flex:1"></span>
      <button class="btn" id="gwRuleAddBtn">${icon("plus")} Add rule</button></div>
      ${bar}${rows}${ruleFormHtml()}</div>`;
  })();

  /* ---- Generated config: one section, two views (design §7) —
   * config.yaml, plus the llmdil_routing.py callback as a second
   * tab when a typed routing rule is enabled. One collapsed row
   * (title, summary values once fetched, Copy / Download / toggle
   * acting on the active tab); the methodology sentences ride
   * info-tips verbatim; a fetch in flight renders "Loading…" in
   * the pre region, never an empty pre; a load failure is a header
   * value with the raw error in its tooltip. ---- */
  const hasRoutingCallback = cfg.routing_rules.some((r) => r.kind !== null && r.enabled);
  const activeGenText = genTab === "routing" ? routingPyText : yamlText;
  const genSummary = activeGenText !== null && !yamlErr
    ? (() => {
        const lines = activeGenText.replace(/\n$/, "").split("\n").length;
        const models = genTab === "yaml" ? (activeGenText.match(/model_name:/g) ?? []).length : 0;
        return `<span style="font-size:12px;color:var(--text2)">Lines: <b style="color:var(--text)">${lines}</b></span>
        ${genTab === "yaml" ? `<span style="font-size:12px;color:var(--text2)">Models: <b style="color:var(--text)">${models}</b></span>` : ""}
        <span style="font-size:12px;color:var(--text2)">Source: current configuration<span class="info-tip" title="The running gateway reads the config file written when it was last (re)started from the Setup Wizard. This preview is regenerated from the current configuration — after edits, the two can differ until the gateway is restarted.">${icon("info")}</span></span>`;
      })()
    : "";
  const genTabs = hasRoutingCallback
    ? `<div class="viewsel" role="tablist" style="margin-top:10px">
        <button data-gen-tab="yaml"${genTab === "yaml" ? ` class="active"` : ""}>config.yaml</button>
        <button data-gen-tab="routing"${genTab === "routing" ? ` class="active"` : ""}>llmdil_routing.py</button>
      </div>`
    : "";
  const yaml = `<div class="card" style="margin-top:12px">
    <div style="display:flex;gap:10px;align-items:center;flex-wrap:wrap">
      <span class="sec-title" style="margin:0">Generated config (preview)</span>
      ${yamlErr ? `<span style="font-size:12px;color:var(--err)" title="${esc(yamlErr)}">Preview: failed to load</span>` : genSummary}
      <span class="spacer" style="flex:1"></span>
      <button class="btn icon-only" id="gwYamlCopyBtn" title="Copy generated config" aria-label="Copy generated config">${icon("copy")}</button>
      <button class="btn icon-only" id="gwYamlDownloadBtn" title="Download ${genTab === "routing" ? "llmdil_routing.py" : "config.yaml"}" aria-label="Download ${genTab === "routing" ? "llmdil_routing.py" : "config.yaml"}">${icon("download")}</button>
      <button class="btn icon-only" id="gwYamlBtn" title="${yamlOpen ? "Hide preview" : "Preview generated config"}" aria-label="${yamlOpen ? "Hide preview" : "Preview generated config"}">${icon(yamlOpen ? "eyeOff" : "eye")}</button>
    </div>
    ${genTabs}
    ${yamlOpen && !yamlErr ? (activeGenText === null
      ? `<pre style="margin-top:10px;padding:12px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;font-family:var(--mono);font-size:11px">Loading\u2026</pre>`
      : `<div style="display:flex;gap:8px;align-items:center;margin-top:10px"><span style="font-size:11px;color:var(--text3);font-family:var(--mono)">${genTab === "routing" ? "llmdil_routing.py" : "config.yaml"}</span>${genTab === "yaml" ? `<span class="info-tip" title="\`api_key: local\` in the generated config is LiteLLM's required non-empty placeholder — it is not a real key.">${icon("info")}</span>` : ""}</div>
         <pre style="margin-top:6px;padding:12px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-word;max-height:420px;overflow:auto">${esc(activeGenText)}</pre>`) : ""}
  </div>`;

  return activationPlanHtml(plan, planUi, "summary") + backendsNote + summary + gwActions + endpoint + openrouter + routing + rules + yaml;
}

export function renderGateway(): string {
  return `${topbar("Gateway")}
  <div class="view"><div id="gwBanner"></div>
  <div id="gwMsg" style="font-size:12px;margin-bottom:8px"></div>
  <div id="gwBody">${loadingHtml("Loading gateway configuration\u2026")}</div></div>`;
}

export async function wireGateway(root: HTMLElement): Promise<() => void> {
  let yamlOpen = false;
  let yamlText: string | null = null;
  let yamlErr = "";
  let disposed = false;
  /** The gateway status from the latest load: the lifecycle title,
   *  actions and PostgreSQL row repaint in place from it (merged
   *  with the shared store's fresher snapshot) without a full body
   *  repaint. */
  let lastStatus: GatewayStatusData | null = null;
  /** Last loaded config (the toggle handler patches against it). */
  let lastCfg: GatewayConfigData | null = null;
  /** Gateway port from the latest load (the admin UI link target). */
  let lastPort = 4000;
  /** LAN address from the latest load (the LAN base URL copy target). */
  let lastLanIp: string | null = null;
  /** Master key, fetched on first Reveal/Copy; kept in the closure
   *  so repaints do not re-read the credential store. */
  let masterKey: string | null = null;
  let keyRevealed = false;
  /** Gateway activation plan, painted at the top of the view. */
  let lastPlan: ActivationPlanData | null = null;
  const planUi = newPlanUi();
  /** Routing section tab (Flow | Groups), the OpenRouter allowlist
   * expandable, and the routing-rule Fragment expandables — view
   * state held outside the repainted DOM. */
  let routingTab: "flow" | "groups" = "flow";
  let orModelsOpen = false;
  const openFragments = new Set<string>();
  /** Models from the latest load (the routing-rule windows and
   *  form warnings resolve each backend's rated context from
   *  them, mirroring the generator). */
  let lastModels: ModelRow[] = [];
  /** Routing-rules form draft + Delete arming + the lazily fetched
   *  per-backend raw flags behind the form's --parallel warning. */
  const ruleForm: RuleFormState = { open: false, editingId: null, target: "", optimalTokens: "", advisory: true };
  const confirmRuleDelete = new Set<string>();
  const ruleRawFlags = new Map<string, string | null>();
  /** Generated config card: active tab + the routing callback
   *  preview text (fetched with the YAML). */
  let genTab: "yaml" | "routing" = "yaml";
  let routingPyText: string | null = null;

  const gwMsg = (text: string, isErr = false): void => {
    const m = root.querySelector("#gwMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  /** Append a manual-command block under the gateway message (a
   * failed start-gateway outcome carries one: the exact commands
   * to start the gateway by hand). #gwMsg lives outside #gwBody,
   * so the block survives the load() repaint that follows. The
   * command is also recorded on the plan UI state so the block's
   * Copy button (data-plan-act="copy-manual") copies this text. */
  const gwShowManual = (cmd: string): void => {
    planUi.manualCommand = cmd;
    const m = root.querySelector("#gwMsg");
    if (m) m.insertAdjacentHTML("beforeend", manualCommandHtml(cmd));
  };

  /** Fetch the master key on first use and cache it in the closure. */
  const ensureMasterKey = async (): Promise<string | null> => {
    if (masterKey !== null) return masterKey;
    const r = await getGatewayMasterKey();
    if (r.error || !r.data.master_key) {
      gwMsg(`Master key unavailable: ${r.error ?? "empty response"}`, true);
      return null;
    }
    masterKey = r.data.master_key;
    return masterKey;
  };

  const load = async (): Promise<void> => {
    const [st, cfg, backs, pr, mdls] = await Promise.all([getGatewayStatus(), getGatewayConfig(), getBackends(), getActivationPlan(), getModels()]);
    if (disposed) return;
    if (!pr.error && pr.data) lastPlan = pr.data;
    if (!mdls.error) lastModels = mdls.data;
    const err = st.error ?? cfg.error;
    // The OpenRouter spend-cap banner (spec §7) rides with the status
    // banner: cap reached, or spend at ≥80% of it.
    const spendBanner = st.error ? "" : openRouterSpendBannerHtml(st.data.openrouter_spend, icon("warn"));
    setBanner(root, "gwBanner", bannerHtml({ live: st.live && cfg.live, error: err })
      + (spendBanner ? `<div style="margin-bottom:12px">${spendBanner}</div>` : ""));
    const b = root.querySelector("#gwBody");
    if (!b) return;
    if (err) { b.innerHTML = emptyHtml(`Gateway configuration unavailable: ${err}`); return; }
    if (!cfg.data) {
      b.innerHTML = emptyHtml(st.live
        ? "The gateway returned no configuration."
        : "No gateway data in sample mode.");
      return;
    }
    lastCfg = cfg.data;
    lastStatus = st.data;
    lastPort = st.data.port || cfg.data.port;
    lastLanIp = st.data.lan_ip ?? null;
    b.innerHTML = gatewayPaint(st.data, cfg.data, backs.data, lastModels, yamlOpen, yamlText, yamlErr, backs.error ?? null, masterKey, keyRevealed, lastPlan, planUi, routingTab, orModelsOpen, openFragments, ruleForm, confirmRuleDelete, ruleRawFlags, genTab, routingPyText);
  };

  /** Fetch the generated config when a Copy/Download click needs
   * it and none is loaded (or the last fetch failed) — the buttons
   * never act on an empty string. Returns the ACTIVE TAB's text
   * (the routing callback tab when selected), or null. */
  const ensureGen = async (): Promise<string | null> => {
    const have = genTab === "routing" ? routingPyText : yamlText;
    if (have !== null && !yamlErr) return have;
    const r = await getGatewayYaml();
    if (disposed) return null;
    if (r.error) { yamlErr = r.error; await load(); return null; }
    yamlText = r.data.yaml;
    routingPyText = r.data.routing_py;
    yamlErr = "";
    return genTab === "routing" ? routingPyText : yamlText;
  };

  /** Serialize a rule back to the stored config shape for the
   *  update_gateway_config merge-patch. The fragment rides its
   *  untouched raw value, so saving an unrelated rule never
   *  rewrites a legacy rule's stored fragment. */
  const serializeRule = (r: GatewayRule): Record<string, unknown> => ({
    id: r.id,
    description: r.description,
    enabled: r.enabled,
    kind: r.kind,
    litellm_fragment: r.fragment_raw === undefined ? null : r.fragment_raw,
    source: r.source,
  });

  /** Save the full rules array through update_gateway_config (the
   *  generic gateway merge-patch — no dedicated command, design
   *  §7) and repaint with the outcome. */
  /** Returns true only when the change was saved (persisted):
   *  callers with an open form close it only on a true result
   *  (design rule 17 — a failed or non-persisted save keeps the
   *  form open with the operator's input intact). */
  const patchRules = async (rules: GatewayRule[], okMsg: string): Promise<boolean> => {
    const r = await updateGatewayConfig({ routing_rules: rules.map(serializeRule) });
    if (disposed) return false;
    let saved = false;
    if (r.error) {
      gwMsg(`Routing rule change failed: ${r.error}`, true);
    } else if (!r.data.persisted) {
      gwMsg("The change was applied to the in-memory configuration only — it was not saved and will be lost when the app restarts.", true);
    } else {
      gwMsg(okMsg);
      saved = true;
    }
    await load();
    return saved;
  };

  /** Open the rule form (add when `editing` is null, else edit that
   *  typed rule). Member backends' raw flags are fetched lazily
   *  for the form's --parallel warning (best-effort). */
  const openRuleForm = async (editing: GatewayRule | null): Promise<void> => {
    if (editing?.kind) {
      ruleForm.editingId = editing.id;
      ruleForm.target = editing.kind.target_group;
      ruleForm.optimalTokens = editing.kind.optimal_tokens !== null ? String(editing.kind.optimal_tokens) : "";
      ruleForm.advisory = editing.kind.advisory;
    } else {
      ruleForm.editingId = null;
      ruleForm.target = lastCfg?.groups[0]?.name ?? "";
      ruleForm.optimalTokens = "";
      ruleForm.advisory = true;
    }
    ruleForm.open = true;
    const ids = new Set((lastCfg?.groups ?? []).flatMap((g) => g.members));
    await Promise.all([...ids].map(async (id) => {
      if (ruleRawFlags.has(id)) return;
      const r = await getBackendConfig(id);
      if (r.data) ruleRawFlags.set(id, r.data.raw_flags);
    }));
    if (!disposed) await load();
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;

    const pt = t.closest("[data-plan-act]") as HTMLElement | null;
    if (pt) {
      await handlePlanAction(pt.getAttribute("data-plan-act") ?? "", planUi, () => { void load(); }, load, lastPlan);
      return;
    }

    const rt = t.closest("[data-routing-tab]") as HTMLElement | null;
    if (rt) {
      routingTab = rt.getAttribute("data-routing-tab") === "groups" ? "groups" : "flow";
      await load();
      return;
    }

    const fr = t.closest("[data-fragment]") as HTMLElement | null;
    if (fr) {
      const id = fr.getAttribute("data-fragment") ?? "";
      if (openFragments.has(id)) openFragments.delete(id); else openFragments.add(id);
      await load();
      return;
    }

    if (t.closest("[data-or-models]")) {
      orModelsOpen = !orModelsOpen;
      await load();
      return;
    }

    if (t.closest("#gwOrCredCopyBtn")) {
      const ref = lastCfg?.openrouter?.cred_ref;
      if (!ref) return;
      try {
        await navigator.clipboard.writeText(ref);
        gwMsg("Credential reference copied to the clipboard.");
      } catch {
        gwMsg("Copy failed — copy the credential reference manually.", true);
      }
      return;
    }

    if (t.closest("#gwYamlCopyBtn")) {
      const text = await ensureGen();
      if (text === null || disposed) return;
      try {
        await navigator.clipboard.writeText(text);
        gwMsg("Generated config copied to the clipboard.");
      } catch {
        gwMsg("Copy failed — open the preview and copy it manually.", true);
      }
      await load();
      return;
    }

    if (t.closest("#gwYamlDownloadBtn")) {
      const text = await ensureGen();
      if (text === null || disposed) return;
      const url = URL.createObjectURL(new Blob([text], { type: "text/yaml" }));
      const a = document.createElement("a");
      a.href = url;
      a.download = genTab === "routing" ? "llmdil_routing.py" : "config.yaml";
      a.click();
      URL.revokeObjectURL(url);
      await load();
      return;
    }

    const gt = t.closest("[data-gen-tab]") as HTMLElement | null;
    if (gt) {
      genTab = gt.getAttribute("data-gen-tab") === "routing" ? "routing" : "yaml";
      await load();
      return;
    }

    if (t.closest("#gwRuleAddBtn")) {
      await openRuleForm(null);
      return;
    }

    const rtog = t.closest("[data-rule-toggle]") as HTMLElement | null;
    if (rtog && lastCfg) {
      const id = rtog.getAttribute("data-rule-toggle") ?? "";
      const rules = lastCfg.routing_rules.map((r) => (r.id === id ? { ...r, enabled: !r.enabled } : r));
      const rule = rules.find((r) => r.id === id);
      await patchRules(rules, rule?.enabled ? `Routing rule ${id} enabled and saved.` : `Routing rule ${id} disabled and saved.`);
      return;
    }

    const redit = t.closest("[data-rule-edit]") as HTMLElement | null;
    if (redit && lastCfg) {
      const rule = lastCfg.routing_rules.find((r) => r.id === (redit.getAttribute("data-rule-edit") ?? ""));
      if (rule) await openRuleForm(rule);
      return;
    }

    const rdel = t.closest("[data-rule-delete]") as HTMLElement | null;
    if (rdel && lastCfg) {
      const id = rdel.getAttribute("data-rule-delete") ?? "";
      if (!confirmRuleDelete.has(id)) {
        confirmRuleDelete.add(id);
        await load();
        return;
      }
      confirmRuleDelete.delete(id);
      await patchRules(lastCfg.routing_rules.filter((r) => r.id !== id), `Routing rule ${id} deleted and saved.`);
      return;
    }

    if (t.closest("#gwRuleCancelBtn")) {
      ruleForm.open = false;
      await load();
      return;
    }

    if (t.closest("#gwRuleSaveBtn")) {
      if (!lastCfg) return;
      const optimalText = ruleForm.optimalTokens.trim();
      const optimal = optimalText === "" ? null : Number(optimalText);
      if (optimal !== null && (!Number.isFinite(optimal) || optimal <= 0 || Math.floor(optimal) !== optimal)) {
        gwMsg("Optimal tokens must be a positive whole number, or empty for the default (half the serving window).", true);
        return;
      }
      const editing = ruleForm.editingId !== null
        ? lastCfg.routing_rules.find((r) => r.id === ruleForm.editingId)
        : undefined;
      const kind: GatewayRuleKind = {
        type: "context-overflow",
        target_group: ruleForm.target,
        optimal_fraction: editing?.kind?.optimal_fraction ?? 0.5,
        optimal_tokens: optimal,
        output_reserve_tokens: editing?.kind?.output_reserve_tokens ?? 1024,
        advisory: ruleForm.advisory,
      };
      const description = `Requests too large for their group are rerouted to ${ruleForm.target}; past the optimal window the serving model advises compacting the conversation`;
      let rules: GatewayRule[];
      if (editing) {
        rules = lastCfg.routing_rules.map((r) => (r.id === editing.id ? { ...r, kind, description } : r));
      } else {
        let id = "context-overflow";
        for (let n = 2; lastCfg.routing_rules.some((r) => r.id === id); n++) id = `context-overflow-${n}`;
        rules = [...lastCfg.routing_rules, { id, description, source: "user", enabled: true, kind, litellm_fragment: undefined, fragment_raw: undefined }];
      }
      // Rule 17: the form closes only when the save persisted.
      // patchRules repaints from the ruleForm draft, so a failed
      // or non-persisted save leaves the form open with the
      // operator's input intact and the error in #gwMsg.
      const saved = await patchRules(rules, editing ? `Routing rule ${editing.id} saved.` : "Routing rule added and saved.");
      if (saved && !disposed) {
        ruleForm.open = false;
        await load();
      }
      return;
    }

    if (t.closest("#gwYamlBtn")) {
      if (yamlOpen) {
        yamlOpen = false;
        await load();
        return;
      }
      // Refetch on every Preview click: a cached copy (or a cached
      // failure) would hide config edits made since the last preview.
      yamlOpen = true;
      yamlText = null;
      routingPyText = null;
      yamlErr = "";
      await load();
      const r = await getGatewayYaml();
      if (disposed) return;
      if (r.error) yamlErr = r.error;
      else {
        yamlText = r.data.yaml;
        routingPyText = r.data.routing_py;
      }
      await load();
      return;
    }

    if (t.closest("#gwKeyRevealBtn")) {
      if (keyRevealed) {
        keyRevealed = false;
        await load();
        return;
      }
      if ((await ensureMasterKey()) === null) return;
      if (disposed) return;
      keyRevealed = true;
      await load();
      return;
    }

    if (t.closest("#gwKeyCopyBtn")) {
      const key = await ensureMasterKey();
      if (key === null || disposed) return;
      try {
        await navigator.clipboard.writeText(key);
        gwMsg("Master key copied to the clipboard.");
      } catch {
        gwMsg("Copy failed \u2014 Reveal the key and copy it manually.", true);
      }
      return;
    }

    if (t.closest("#gwBaseUrlCopyBtn")) {
      try {
        await navigator.clipboard.writeText(`http://127.0.0.1:${lastPort}/v1`);
        gwMsg("Base URL copied to the clipboard.");
      } catch {
        gwMsg("Copy failed \u2014 copy the base URL manually.", true);
      }
      return;
    }

    if (t.closest("#gwLanUrlCopyBtn")) {
      if (!lastLanIp) return;
      try {
        await navigator.clipboard.writeText(`http://${lastLanIp}:${lastPort}/v1`);
        gwMsg("LAN base URL copied to the clipboard.");
      } catch {
        gwMsg("Copy failed \u2014 copy the LAN base URL manually.", true);
      }
      return;
    }

    if (t.closest("#gwAdminUiBtn")) {
      const r = await openUrl(`http://127.0.0.1:${lastPort}/ui`);
      if (disposed) return;
      if (r.error) gwMsg(`Could not open the admin UI: ${r.error}`, true);
      return;
    }

    if (t.closest("#gwOrToggle")) {
      const or = lastCfg?.openrouter;
      if (!or) return;
      const btn = t.closest("#gwOrToggle") as HTMLButtonElement;
      btn.disabled = true;
      const r = await updateGatewayConfig({ openrouter: { enabled: !or.enabled } });
      if (disposed) return;
      if (r.error) {
        gwMsg(`OpenRouter tier change failed: ${r.error}`, true);
      } else if (!r.data.persisted) {
        gwMsg("The change was applied to the in-memory configuration only \u2014 it was not saved and will be lost when the app restarts.", true);
      } else {
        gwMsg(or.enabled ? "OpenRouter tier disabled and saved." : "OpenRouter tier enabled and saved.");
      }
      await load();
    }

    /** Shared save for the Gateway view's permanent fields
     *  (port, OpenRouter credential/cap/allowlist, per-group
     *  fallbacks). Rule 17: on success the repaint displays the
     *  saved value in its new state; on failure — or a
     *  non-persisted apply — there is NO repaint, so the
     *  operator's input stays intact in the field and the error
     *  shows in #gwMsg (which lives outside the repainted body).
     *  A repaint on failure would discard the typed value and
     *  redisplay the old saved one. */
    const gwSave = async (patch: Record<string, unknown>, okText: string): Promise<void> => {
      const r = await updateGatewayConfig(patch);
      if (disposed) return;
      if (r.error) { gwMsg(`Change not saved: ${r.error}`, true); return; }
      if (!r.data.persisted) { gwMsg("The change was applied to the in-memory configuration only — it was not saved and will be lost when the app restarts.", true); return; }
      gwMsg(okText);
      await load();
    };

    if (t.closest("#gwPortApply")) {
      const port = Math.round(Number((root.querySelector("#gwPortInput") as HTMLInputElement | null)?.value));
      if (!Number.isFinite(port) || port < 1 || port > 65535) {
        gwMsg("Gateway port must be between 1 and 65535.", true);
        return;
      }
      await gwSave({ port }, "Gateway port saved — applies at the next gateway start.");
      return;
    }

    if (t.closest("#gwOrCredSave")) {
      const v = ((root.querySelector("#gwOrCredInput") as HTMLInputElement | null)?.value ?? "").trim();
      await gwSave({ openrouter: { cred_ref: v === "" ? null : v } },
        v === "" ? "Credential reference cleared and saved." : "Credential reference saved.");
      return;
    }

    if (t.closest("#gwOrCapSave")) {
      const cap = Number((root.querySelector("#gwOrCapInput") as HTMLInputElement | null)?.value);
      if (!Number.isFinite(cap) || cap < 0) {
        gwMsg("Daily cap must be a number of dollars, 0 or more.", true);
        return;
      }
      await gwSave({ openrouter: { daily_cap_usd: cap } }, "OpenRouter daily cap saved.");
      return;
    }

    if (t.closest("#gwOrAllowSave")) {
      const raw = (root.querySelector("#gwOrAllowInput") as HTMLTextAreaElement | null)?.value ?? "";
      const list = [...new Set(raw.split(/[\n,]/).map((s) => s.trim()).filter(Boolean))];
      await gwSave({ openrouter: { model_allowlist: list } }, "OpenRouter model allowlist saved.");
      return;
    }

    const fbSave = t.closest("[data-group-fallbacks-save]") as HTMLElement | null;
    if (fbSave && lastCfg) {
      const name = fbSave.getAttribute("data-group-fallbacks-save") ?? "";
      const input = root.querySelector(`[data-group-fallbacks-input="${name}"]`) as HTMLInputElement | null;
      const list = [...new Set((input?.value ?? "").split(",").map((s) => s.trim()).filter(Boolean))];
      // Patch the full groups array with only this group's chain
      // changed; the stored fields ride through verbatim (the
      // derived enabled/enabled_members are not stored fields).
      const groups = lastCfg.groups.map((g) => ({
        name: g.name, members: g.members, strategy: g.strategy, purpose: g.purpose,
        fallbacks: g.name === name ? list : g.fallbacks,
      }));
      await gwSave({ groups }, `Fallbacks for ${name} saved — applies at the next gateway start.`);
      return;
    }
  };
  const onBindChange = async (ev: Event): Promise<void> => {
    const sel = ev.target as HTMLElement;
    if (!sel.closest("#gwBindSelect")) return;
    const host = (sel as HTMLSelectElement).value;
    const r = await setGatewayBindHost(host);
    if (r.error) {
      gwMsg(`Network access not saved: ${r.error}`, true);
      return;
    }
    gwMsg("Network access saved — applies at the next gateway restart; the start-gateway step also reconciles the Windows firewall rule.");
    await load();
  };
  /** Rule-form field changes update the draft in the closure (the
   *  form re-renders from it); Target/Optimal changes repaint so
   *  the §6.2 warnings track the draft. */
  const onRuleFormChange = async (ev: Event): Promise<void> => {
    const el = ev.target as HTMLElement;
    if (el.closest("#gwRuleTarget")) {
      ruleForm.target = (el as HTMLSelectElement).value;
      await load();
    } else if (el.closest("#gwRuleOptimal")) {
      ruleForm.optimalTokens = (el as HTMLInputElement).value;
      await load();
    } else if (el.closest("#gwRuleAdvisory")) {
      ruleForm.advisory = (el as HTMLInputElement).checked;
    }
  };
  root.addEventListener("click", onClick);
  root.addEventListener("change", onBindChange);
  root.addEventListener("change", onRuleFormChange);

  await load();

  // Lifecycle buttons (gateway Start/Stop/Restart, PostgreSQL Stop)
  // run through the shared runner: Restart is ONE server-side
  // operation (no client-side stop, no client activationComplete —
  // the server clears the plan), Stop stops the gateway only, and
  // Start/Restart offer the MCP credential prompt first.
  const unwireLc = wireLifecycleActions(root, (r: LifecycleResult) => {
    if (disposed) return;
    gwMsg(r.message, !r.ok && !r.cancelled);
    if (r.manualCommand) gwShowManual(r.manualCommand);
    void load();
  }, { origin: "ui" });
  // The lifecycle title, actions and PostgreSQL row repaint in place
  // from the shared store (every restart, from any surface, reads
  // "restarting…" here), without the full body repaint the hover
  // rule below guards.
  const paintLc = (): void => {
    if (!lastStatus) return;
    const t = root.querySelector("#gwLcTitle");
    if (t) setHtml(t, gatewayLcTitleHtml(lastStatus));
    const a = root.querySelector("#gwLcActions");
    if (a) setHtml(a, gatewayLcActionsHtml(lastStatus));
    const p = root.querySelector("#gwPgRow");
    if (p) setHtml(p, gatewayPgRowHtml(lastStatus));
  };
  const unsubscribeStore = serviceStore.subscribe(() => { if (!disposed) paintLc(); });

  // Status poll: a gateway that dies while the view is open must show
  // as down without a manual refresh. The repaint replaces #gwBody's
  // innerHTML wholesale, which destroys the node under the pointer —
  // a native title tooltip on an info-tip inside the body is
  // cancelled before it shows or dismissed while it is read (the
  // Routing test traces tip was reported exactly this way: cursor
  // changes, no tooltip). While the pointer hovers the body the
  // user is reading it, so the periodic repaint waits for the next
  // cycle; click-driven loads are unaffected.
  const timer = window.setInterval(() => {
    if (!disposed && !root.querySelector("#gwBody:hover")) void load();
  }, 5000);

  return () => {
    disposed = true;
    unwireLc();
    unsubscribeStore();
    window.clearInterval(timer);
    root.removeEventListener("click", onClick);
    root.removeEventListener("change", onBindChange);
    root.removeEventListener("change", onRuleFormChange);
  };
}

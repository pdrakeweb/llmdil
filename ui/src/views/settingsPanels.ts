/* Settings view panels: MXC policy, app updates, supervision, log
   shipping, component updates, remote access (split from static.ts). */

import type { AdminApiAuditEntry, AdminApiPairing, AdminApiStatus, AdminApiUser, AppUpdateInfo, ComponentUpdateCheck, LogReceiverInfo, LogShipperInfo, MxcActivityData, MxcPolicyData, ServicesInfo, TaskRecordWire } from "../api";
import { fmtCount, stackedBar, trim1 } from "../charts";
import { esc } from "../html";
import { icon } from "../icons";
import { META, lifecycleChip, lifecycleLabel, logReceiverState, renderLifecycleActions, serviceRowState, serviceStore, targetKey } from "../lifecycle";
import type { LifecycleTarget, ServiceSnapshot } from "../lifecycle";
import { enableSwitch } from "../switch";
import { C, cadenceBucket, fmtDate, fmtTime, loadingHtml } from "./common";
import { fmtDateTime } from "./logs";

/* ---------------- Settings ---------------- */

export function asRecord(v: unknown): Record<string, unknown> | null {
  return v !== null && typeof v === "object" && !Array.isArray(v)
    ? (v as Record<string, unknown>)
    : null;
}

function primStr(v: unknown): string | null {
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  return null;
}

/** Defensive one-line summary of a config value: primitives verbatim,
    arrays/objects as counts — never invented detail. Numbers route
    through the shared formatter (one decimal, trailing .0 dropped,
    thousands separators) — this path is the settings view's leak
    point for raw full-precision values. */
function summarizeValue(v: unknown): string {
  if (typeof v === "number") return trim1(v);
  const p = primStr(v);
  if (p !== null) return p;
  if (Array.isArray(v)) return `${v.length} item${v.length === 1 ? "" : "s"}`;
  const r = asRecord(v);
  if (r) return `{ ${Object.keys(r).length} fields }`;
  return "\u2014";
}

/** Config keys are storage names, not labels — humanize them for
 *  display (known keys map to their noun label; anything else gets
 *  its underscores spaced and its first letter capitalized). */
const SETTINGS_LABEL: Record<string, string> = {
  poll_interval_ms: "Poll interval", interval_ms: "Interval", poll_ms: "Poll interval", interval: "Interval",
};
function humanLabel(key: string): string {
  return SETTINGS_LABEL[key] ?? key.replace(/_/g, " ").replace(/^\w/, (c) => c.toUpperCase());
}
function humanEntries(rec: Record<string, unknown>, limit = 24): { key: string; value: string }[] {
  return Object.entries(rec).slice(0, limit).map(([key, v]) => ({ key: humanLabel(key), value: summarizeValue(v) }));
}

type SettingsRow = { key: string; value: string; tip?: string; copy?: string };

/** Label: Value rows. An empty list renders NOTHING (principle 13 —
 *  no "Nothing reported." placeholder). A row may carry an info-tip
 *  (narrative) and a Copy button (addresses: paths). */
function settingsRows(rows: SettingsRow[]): string {
  if (!rows.length) return "";
  return `<div class="kv" style="gap:10px">${rows.map((r) => `<div style="display:flex;justify-content:space-between;gap:16px"><span class="k">${esc(r.key)}${r.tip ? `<span class="info-tip" title="${esc(r.tip)}">${icon("info")}</span>` : ""}</span><span class="v" style="font-size:12px;text-align:right">${esc(r.value)}${r.copy !== undefined ? ` <button class="btn" data-copy-value="${esc(r.copy)}" title="Copy" aria-label="Copy value" style="padding:1px 7px;font-size:11px">${icon("copy")}</button>` : ""}</span></div>`).join("")}</div>`;
}

const MXC_MODES: { mode: string; label: string; def: string }[] = [
  { mode: "learning", label: "Learning", def: "Learning — observe and log only; nothing is blocked." },
  { mode: "enforced", label: "Enforced", def: "Enforced — execution outside the policy is blocked." },
  { mode: "disabled", label: "Disabled", def: "Disabled — sandbox off; tool execution is not mediated." },
];

const FS_ACCESS_LABEL: Record<string, string> = { readwrite: "read-write" };
const ACTIVITY_KIND_LABEL: Record<string, string> = {
  FileRead: "file read", FileWrite: "file write", Network: "network",
};

/** The Active policy body: the policy's actual rules, with the
 *  provenance in the heading's info-tip — a fetch failure is
 *  "unavailable — <error>", never conflated with a genuinely absent
 *  policy (which renders no block at all). The three rule lists are
 *  ONE "Policy rules" block behind a segmented filter whose keys
 *  carry the counts; a long list is complete inside a collapsed
 *  expandable — truncation is gone, every rule is reachable. */
function mxcPolicyInner(
  policyR: { data: MxcPolicyData | null; error?: string; live: boolean },
  rulesFilter: "fs" | "net" | "cred",
  rulesOpen: Set<string>,
  policyFile: string | null,
): string {
  if (policyR.error) {
    return `<div style="font-size:12px;color:var(--err);margin-top:10px">Policy unavailable \u2014 ${esc(policyR.error)}</div>`;
  }
  const data = policyR.data;
  if (!data || !data.policy) return "";
  const p = data.policy;
  const srcLine = data.source === "default"
    ? "No policy file exists \u2014 the built-in default policy is in effect."
    : "Loaded from the configured policy file.";
  const basics: SettingsRow[] = [
    ...(policyFile !== null ? [{ key: "Policy file", value: policyFile, copy: policyFile } as SettingsRow] : []),
    { key: "Policy version", value: String(p.version) },
    { key: "Default", value: p.default_deny ? "deny" : "allow", ...(p.default_deny ? { tip: "anything not explicitly allowed is denied" } : {}) },
    { key: "UI access", value: p.ui_access ? "enabled" : "disabled" },
  ];
  const lists: { id: "fs" | "net" | "cred"; label: string; rows: SettingsRow[] }[] = [
    { id: "fs", label: "Filesystem", rows: (p.filesystem ?? []).map((r) => ({ key: r.path, value: FS_ACCESS_LABEL[r.access] ?? r.access })) },
    { id: "net", label: "Network", rows: (p.network ?? []).map((r) => ({ key: r.ports && r.ports.length ? `${r.host}:${r.ports.join(",")}` : r.host, value: r.decision })) },
    { id: "cred", label: "Credentials", rows: (p.credentials ?? []).map((r) => ({ key: r.name, value: r.target })) },
  ];
  const active = lists.find((l) => l.id === rulesFilter) ?? lists[0];
  const LONG = 12;
  const listHtml = active.rows.length > LONG && !rulesOpen.has(active.id)
    ? `<button class="btn" data-rules-toggle="${active.id}" title="Show all ${active.rows.length} rules" aria-label="Show all ${active.rows.length} rules" style="padding:2px 8px;font-size:11px">${icon("chevR")}</button>`
    : `${active.rows.length > LONG ? `<button class="btn" data-rules-toggle="${active.id}" title="Hide rules" aria-label="Hide rules" style="padding:2px 8px;font-size:11px;margin-bottom:6px">${icon("chevD")}</button>` : ""}${settingsRows(active.rows)}`;
  return `<div style="font-size:12px;color:var(--text2);margin:14px 0 6px">Active policy<span class="info-tip" title="${esc(srcLine)}">${icon("info")}</span></div>
    ${settingsRows(basics)}
    <div style="font-size:12px;color:var(--text2);margin:12px 0 6px">Policy rules</div>
    <div class="viewsel" role="tablist" style="margin-bottom:8px">${lists.map((l) => `<button data-rules-filter="${l.id}"${l.id === active.id ? ` class="active"` : ""}>${l.label} (${l.rows.length})</button>`).join("")}</div>
    ${listHtml}`;
}

/** The Recent activity body: structured rows (time, decision chip
 *  colored per principle 11, what was accessed, count) under a
 *  decision-mix segmented bar; the entry description rides the
 *  row's tooltip. Fetch failure stays distinct from an empty
 *  report, and an empty report renders no block at all. */
function mxcActivityInner(activityR: { data: MxcActivityData | null; error?: string; live: boolean }): string {
  if (activityR.error) {
    return `<div style="font-size:12px;color:var(--err);margin-top:10px">Activity unavailable \u2014 ${esc(activityR.error)}</div>`;
  }
  const data = activityR.data;
  if (!data || !data.entries.length) return "";
  const totals = new Map<string, number>();
  for (const e of data.entries) totals.set(e.decision, (totals.get(e.decision) ?? 0) + (e.count || 1));
  const totalAll = [...totals.values()].reduce((a, b) => a + b, 0);
  const decColor = (d: string): string => d === "allow" ? C.green : d === "deny" ? "#f85149" : d === "prompt" ? C.amber : "#6d7686";
  const mixBar = totalAll > 0
    ? `<div style="margin-bottom:6px">${stackedBar([...totals.entries()].map(([d, n]) => ({ pct: (n / totalAll) * 100, color: decColor(d), label: `${d}: ${fmtCount(n)}` })), 600, 8)}
       <div class="legend" style="margin-top:4px">${[...totals.entries()].map(([d, n]) => `<span><span class="sw" style="background:${decColor(d)}"></span>${esc(d)} ${fmtCount(n)}</span>`).join("")}</div></div>`
    : "";
  const decChip = (d: string): string => d === "allow" ? "badge-ok" : d === "deny" ? "badge-err" : d === "prompt" ? "badge-warn" : "";
  const rows = data.entries.map((e) => {
    const kind = e.kind ? (ACTIVITY_KIND_LABEL[e.kind] ?? e.kind) : "";
    const target = e.path_or_host ? e.path_or_host + (e.port !== null ? `:${e.port}` : "") : "";
    const what = [kind, target].filter(Boolean).join(" \u00b7 ");
    return `<div style="padding:4px 0;border-bottom:1px solid var(--border);font-size:12px"${e.description ? ` title="${esc(e.description)}"` : ""}>
      <span title="${esc(fmtDateTime(e.timestamp))}">${esc(fmtTime(e.timestamp))}</span>
      <span class="chip ${decChip(e.decision)}">${esc(e.decision)}</span>
      ${what ? `<span>${esc(what)}</span>` : ""}
      ${e.count > 1 ? `<span style="color:var(--text3)">\u00d7${e.count}</span>` : ""}
    </div>`;
  }).join("");
  return `<div style="font-size:12px;color:var(--text2);margin:14px 0 6px">Recent activity</div>${mixBar}${rows}`;
}

export type SettingsTab = "general" | "remote" | "updates" | "notifications" | "mxc" | "supervision";

export interface SettingsUi {
  tab: SettingsTab;
  rulesFilter: "fs" | "net" | "cred";
  rulesOpen: Set<string>;
  allOpen: boolean;
}

/** The LLMdil self-update block at the top of the Updates tab:
 *  the two operator switches, the check state as Label: Value rows,
 *  and the check / download actions. Fed by the app_update_status
 *  payload, not the config: last-checked, available, and staged
 *  are runtime state. The per-component pins below it are a
 *  separate mechanism (the wizard's component updates) and stay
 *  separate here too. Applying a staged update is the shell
 *  banner's Restart, not a control on this tab. */
export function appUpdateInner(upd: AppUpdateInfo | null, live: boolean): string {
  const head = `<div style="font-size:12px;color:var(--text2)">LLMdil updates<span class="info-tip" title="Checks GitHub Releases for a newer LLMdil. Auto-update downloads and verifies the installer in the background; it is applied only when you restart from the banner — the running installation is never replaced in place.">${icon("info")}</span></div>`;
  if (!live) {
    return `${head}<div style="font-size:12px;color:var(--text2);margin-top:6px">Update checking is available in the installed app.</div>`;
  }
  if (!upd) {
    return `${head}<div style="font-size:12px;color:var(--text2);margin-top:6px">Update state unavailable.</div>`;
  }
  const fmtMs = (ms: number): string => fmtDateTime(new Date(ms).toISOString());
  const switchRow = (key: string, label: string, desc: string, on: boolean, disabled: boolean): string =>
    `<div style="display:flex;justify-content:space-between;align-items:center;gap:16px;padding:4px 0;border-bottom:1px solid var(--border)">
      <span class="k">${esc(label)}<span class="info-tip" title="${esc(desc)}">${icon("info")}</span></span>
      ${enableSwitch({ on, name: label, attrs: `data-appupd-toggle="${key}"`, disabled })}</div>`;
  const rows: { key: string; value: string }[] = [
    { key: "Current version", value: upd.current_version },
  ];
  if (upd.last_checked_unix_ms !== null) {
    const outcome = upd.last_check_outcome === "update_available" ? "update available"
      : upd.last_check_outcome === "up_to_date" ? "up to date"
      : upd.last_check_outcome === "failed" ? `failed${upd.last_check_error ? ` — ${upd.last_check_error}` : ""}`
      : "";
    rows.push({ key: "Last checked", value: `${fmtMs(upd.last_checked_unix_ms)}${outcome ? ` · ${outcome}` : ""}` });
  }
  if (upd.available) rows.push({ key: "Available version", value: upd.available.version });
  if (upd.staged) rows.push({ key: "Staged", value: `${upd.staged.version} · staged ${fmtMs(upd.staged.staged_at_unix_ms)} — restart to apply` });
  const applyNote = upd.last_apply && !upd.last_apply.ok
    ? `<div style="font-size:12px;color:var(--err);margin-top:6px">Update to ${esc(upd.last_apply.version)} failed: ${esc(upd.last_apply.message)}</div>`
    : "";
  const stageErr = upd.last_stage_error
    ? `<div style="font-size:12px;color:var(--err);margin-top:6px">Download failed: ${esc(upd.last_stage_error)}</div>`
    : "";
  const canDownload = !!upd.available && !upd.staged && !upd.auto_update;
  return `${head}
    ${switchRow("checking", "Update checking", "Check GitHub Releases for a newer LLMdil at startup and every 6 hours while running.", upd.check_enabled, false)}
    ${switchRow("auto", "Auto-update", "Download and verify a new release automatically when a check finds one. Applying still takes a restart.", upd.auto_update, !upd.check_enabled)}
    <div style="margin-top:6px">${settingsRows(rows)}</div>
    ${applyNote}${stageErr}
    <div class="card-actions" style="margin-top:8px"><button class="btn" data-appupd-check>${icon("search")} Check now</button>
    ${canDownload ? `<button class="btn" data-appupd-download>${icon("download")} Download ${esc(upd.available!.version)}</button>` : ""}</div>`;
}

/** One Settings section, six tab views (principle 2) — page length
 *  is the longest tab, not the sum of six stacked cards. The
 *  Notifications tab renders only when notification settings
 *  exist. All narrative rides info-tips; empty blocks render
 *  nothing. */

/** The Supervision card body: mode toggle, shim/account state, and
 *  one row per managed service with its registration + SCM state and
 *  Register/Unregister actions. Registration is the per-service
 *  opt-in; the mode alone changes nothing until a service is
 *  registered (docs/windows-services-design.md). */
export function supervisionInner(svcR: { data: ServicesInfo; error?: string; live: boolean }): string {
  if (svcR.error) {
    return `<div style="font-size:12px;color:var(--err)">Supervision unavailable — ${esc(svcR.error)}</div>`;
  }
  if (!svcR.live) {
    return `<div style="font-size:12px;color:var(--text2)">Service supervision is available in the installed app.</div>`;
  }
  const info = svcR.data;
  const mode = info.mode === "service" ? "service" : "process";
  const rows: { key: string; value: string }[] = [
    { key: "Service account", value: info.account ?? "—" },
    { key: "Service host (llm-service-host.exe)", value: info.host_present ? info.host_path : `missing — expected at ${info.host_path}` },
  ];
  const svcRows = info.services.map((s) => {
    // The shared lifecycle vocabulary (SCM state + exit code mapped
    // server-side to `lifecycle`; fromScm otherwise), with the store's
    // pending overlay, so a Start/Stop/Restart here — or anywhere —
    // reads as the transition at once (UI-7, SCM-15, OS-13).
    const target = serviceRowTarget(s);
    const key = targetKey(target);
    // The server routes a registered gateway/backend through the SCM
    // only in service mode (the log receiver is always an SCM
    // service). In process mode the registration's SCM state is not
    // the target's state, and its Start/Stop would act on the process
    // instead: show it labelled as the service's state, with no buttons.
    const scmRouted = s.registered && (mode === "service" || target.kind === "log-receiver");
    const pid = s.child_pid ?? s.pid;
    let chip: string;
    let lc = "";
    if (scmRouted || !s.registered) {
      const st = serviceStore.effective(key, serviceRowState(s));
      const tr = serviceStore.transitionOf(key, s.transition ?? null);
      chip = lifecycleChip(st, {
        transition: tr,
        failure: s.failure_reason ?? null,
        detail: pid !== null && pid !== undefined && META[st].isLive ? `pid ${pid}` : undefined,
      });
      if (scmRouted) {
        const row = target.kind === "backend" ? serviceStore.get().backends.find((b) => b.id === target.id) : undefined;
        lc = renderLifecycleActions(target, st, row ? { enabled: row.enabled, configured: row.configured } : {});
      }
    } else {
      const scmSt = serviceRowState(s);
      chip = `<span style="color:var(--text3)" title="Process mode: the app runs this as a process; this is the registered Windows service's own state">service: ${esc(lifecycleLabel(scmSt))}</span>`;
    }
    const exit = s.exit_code ? ` <span style="color:var(--text3)" title="${esc(s.failure_reason ?? "The service-specific exit code of the last stop")}">exit ${s.exit_code}</span>` : "";
    const btn = s.registered
      ? `<button class="btn" data-svc-unregister="${esc(s.target)}">Unregister</button>`
      : `<button class="btn" data-svc-register="${esc(s.target)}" ${info.host_present ? "" : "disabled"}>Register</button>`;
    const name = s.target === "gateway" ? "Gateway" : s.target === "log-receiver" ? "Log receiver" : s.target;
    return `<div style="display:flex;justify-content:space-between;align-items:center;gap:16px;padding:4px 0;border-bottom:1px solid var(--border);font-size:12px">
      <span>${esc(name)} <span style="color:var(--text3)">${esc(s.service_name)}</span></span>
      <span style="display:flex;align-items:center;gap:10px;flex-wrap:wrap;justify-content:flex-end">${chip}${exit}${lc}${btn}</span></div>`;
  }).join("");
  const anyUnregistered = info.services.some((s) => !s.registered);
  const regNote = anyUnregistered
    ? `<div style="font-size:11px;color:var(--text3);margin-top:10px">Registration runs elevated (one UAC prompt per service); Windows then asks for the account password in its own credential dialog. Windows stores the password as the service's logon credential; the app does not keep it.</div>`
    : "";
  return `${settingsRows(rows)}
    <div class="card-actions">${["process", "service"].map((m) =>
      `<button class="btn${mode === m ? " primary" : ""}" data-sup-mode="${m}">${m === "process" ? "Process" : "Service"}</button>`).join("")}</div>
    <div style="margin-top:10px">${svcRows}</div>
    ${regNote}`;
}

/** The lifecycle target a services_list row controls. */
function serviceRowTarget(s: { target: string; kind?: string }): LifecycleTarget {
  if (s.kind === "gateway" || s.target === "gateway") return { kind: "gateway" };
  if (s.kind === "log-receiver" || s.target === "log-receiver") return { kind: "log-receiver" };
  return { kind: "backend", id: s.target };
}

/** A signature of the Supervision tab's lifecycle-relevant state, so
 *  a store refresh repaints it only when something changed. */
export function supervisionSignature(snap: ServiceSnapshot): string {
  const rows = (snap.services?.services ?? []).map((s) => {
    const key = targetKey(serviceRowTarget(s));
    return `${key}:${s.registered}:${serviceStore.effective(key, serviceRowState(s))}:${s.transition?.kind ?? ""}:${s.exit_code ?? ""}:${s.pid ?? ""}`;
  });
  const lr = snap.logReceiver;
  const lrSig = lr ? `${lr.registered}:${serviceStore.effective("log-receiver", logReceiverState(lr))}:${lr.exit_code ?? ""}:${lr.ingest_ok ?? ""}` : "";
  return `${rows.join("|")}#${lrSig}`;
}

/** What one log-streaming source state means, in the card's words.
 *  Only `active` claims the source is shipping. */
const LOG_SOURCE_STATE_TEXT: Record<string, string> = {
  active: "shipping",
  disabled: "off — not shipped",
  file_missing: "on, but its log file does not exist yet",
  waiting: "on, nothing captured yet",
  unavailable: "on, but no event feed in this process",
  local_only: "on, but held: ships only to a local (loopback) endpoint",
  not_running: "on — shipper not running",
};

/** The log-streaming source switches, in card order: the key under
 *  `log_streaming.sources` in the config, and the source name the
 *  shipper reports (and stamps on the wire). */
const LOG_SOURCE_SWITCHES: { key: string; source: string; tip: string }[] = [
  { key: "wizard", source: "wizard", tip: "Setup wizard step output (logs/wizard.log)." },
  { key: "optimize", source: "optimize", tip: "Optimize pipeline output (logs/optimize.log)." },
  { key: "gateway", source: "gateway", tip: "The gateway's stdout and stderr (logs/gateway.stdout.log, logs/gateway.stderr.log)." },
  { key: "service_manage", source: "service-manage", tip: "The elevated service helper's outcomes (logs/service-manage.log)." },
  { key: "audit", source: "audit", tip: "The Admin API audit log (logs/admin-api-audit.jsonl). Records carry usernames and source addresses, so this source ships only when the endpoint is on this machine." },
  { key: "app", source: "app", tip: "App events: operation events and backend, gateway and activation state changes." },
  { key: "backends", source: "backends", tip: "Each backend's own output, as backend:<id>. Off by default: model servers are verbose. One switch for every backend." },
];

function logSourceStateText(state: string, records: number): string {
  return `${LOG_SOURCE_STATE_TEXT[state] ?? state}${records > 0 ? ` · ${records} record${records === 1 ? "" : "s"}` : ""}`;
}

/** The Log shipper block under the Log receiver card: the shipper's
 *  state, then one row per source with its switch and what it is
 *  actually doing — so a source that is on but not shipping (file
 *  missing, held, shipper not running) reads as exactly that. A
 *  switch saves `log_streaming.sources` through update_config and
 *  the shipper restarts with it at once; nothing waits for an app
 *  restart. */
function logShipperInner(shipper: LogShipperInfo): string {
  const summary = shipper.running
    ? `${shipper.state} · ${shipper.delivered ?? 0} delivered · ${shipper.queued ?? 0} queued`
    : (shipper.configured?.enabled ? "not running" : "off (log_streaming.enabled is false)");
  const dropped = (shipper.dropped_queue ?? 0) + (shipper.dropped_spool ?? 0) + (shipper.dropped_events ?? 0);
  const head: SettingsRow[] = [
    { key: "Shipper", value: summary },
    ...(dropped > 0 ? [{ key: "Dropped", value: `${shipper.dropped_queue ?? 0} queue · ${shipper.dropped_spool ?? 0} spool · ${shipper.dropped_events ?? 0} events` } as SettingsRow] : []),
  ];
  const rowOf = (source: string) => shipper.sources.find((r) => r.source === source);
  const perBackend = shipper.sources.filter((r) => r.source.startsWith("backend:"));
  const configured = shipper.configured?.sources;
  const switchRows = LOG_SOURCE_SWITCHES.map((sw) => {
    const row = rowOf(sw.source);
    // Per-backend logs report one row per backend (listed below);
    // the switch row itself says only whether the source is on.
    const stateText = row
      ? logSourceStateText(row.state, row.records)
      : (sw.source === "backends" && perBackend.length > 0 ? `on — ${perBackend.length} backend${perBackend.length === 1 ? "" : "s"} below` : "");
    const on = configured ? configured[sw.key] === true : (row ? row.state !== "disabled" : false);
    const control = configured
      ? enableSwitch({ on, name: `Log source ${sw.source}`, attrs: `data-lr-source="${esc(sw.key)}"` })
      : "";
    return `<div style="display:flex;justify-content:space-between;gap:16px;align-items:center"><span class="k">${esc(sw.source)}<span class="info-tip" title="${esc(sw.tip)}">${icon("info")}</span></span><span class="v" style="font-size:12px;text-align:right;display:inline-flex;align-items:center;gap:10px"><span data-lr-source-state="${esc(sw.key)}">${esc(stateText)}</span>${control}</span></div>`;
  }).join("");
  const backendRows: SettingsRow[] = perBackend.map((r) => ({ key: r.source, value: logSourceStateText(r.state, r.records) }));
  const note = shipper.note
    ? `<div data-lr-shipper-note style="font-size:12px;color:var(--err);margin-top:6px">Log streaming is on but the shipper is not running: ${esc(shipper.note)}</div>`
    : "";
  const pending = shipper.restart_pending
    ? `<div data-lr-shipper-pending style="font-size:12px;color:var(--warn);margin-top:6px">Saved log-streaming settings differ from what the shipper is running with. Restart LLMdil to apply them.</div>`
    : "";
  return `<div data-lr-shipper style="font-size:12px;color:var(--text2);margin-top:16px">Log shipper<span class="info-tip" title="One row per log-streaming source. Only a source marked shipping is being sent to the receiver. A switch saves the source's setting and the shipper restarts with it at once; lines already read are kept and delivered. Plain http:// is accepted only for an endpoint on this machine.">${icon("info")}</span></div>
    ${settingsRows(head)}
    ${note}
    <div class="kv" style="gap:10px;margin-top:10px">${switchRows}</div>
    ${backendRows.length ? `<div style="margin-top:10px">${settingsRows(backendRows)}</div>` : ""}
    ${pending}`;
}

/** The Log receiver block on the Supervision tab: deployment state
 *  of the Prometheus-local log receiver service (bundled in the
 *  installer beside the app) and its three deliberate actions —
 *  Deploy/Repair and Remove run the receiver's own subcommands
 *  elevated; Flip endpoint is a separate config save, and the
 *  shipper restarts onto the new endpoint at once. A read token generated by the deploy that just ran
 *  shows once here, for Secure Vault filing. */
export function logReceiverInner(
  lrR: { data: LogReceiverInfo; error?: string; live: boolean },
  newToken: string | null,
): string {
  if (lrR.error) {
    return `<div style="font-size:12px;color:var(--text2);margin-top:16px">Log receiver</div>
    <div style="font-size:12px;color:var(--err)">Log receiver status unavailable — ${esc(lrR.error)}</div>`;
  }
  if (!lrR.live) {
    return `<div style="font-size:12px;color:var(--text2);margin-top:16px">Log receiver</div>
    <div style="font-size:12px;color:var(--text2)">Log receiver deployment is available in the installed app.</div>`;
  }
  const info = lrR.data;
  // The receiver's lifecycle state (SCM state + exit code, downgraded
  // to "not responding" when the ingest port refuses connections),
  // with the shared chip and Start/Stop/Restart (OS-12).
  const lst = serviceStore.effective("log-receiver", logReceiverState(info));
  const ltr = serviceStore.transitionOf("log-receiver", info.transition ?? null);
  const ingestNote = info.ingest_port !== undefined
    ? `Ingest port ${info.ingest_port}${info.ingest_ok === false ? " refuses connections" : info.ingest_ok ? " accepting" : ""}`
    : undefined;
  const lrState = info.registered
    ? `<div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap;font-size:12px;margin:6px 0">${esc(info.service_name)} ${lifecycleChip(lst, { transition: ltr, failure: info.failure_reason ?? null, detail: ingestNote })}${info.exit_code ? ` <span style="color:var(--text3)">exit ${info.exit_code}</span>` : ""}${info.failure_reason && lst === "failed" ? ` <span style="color:var(--err)">${esc(info.failure_reason)}</span>` : ""}<span class="spacer" style="flex:1"></span>${renderLifecycleActions({ kind: "log-receiver" }, lst)}</div>`
    : "";
  const rows: SettingsRow[] = [
    { key: "Service", value: info.registered ? info.service_name : `${info.service_name} · not deployed` },
    { key: "Receiver binary", value: info.receiver_present ? info.receiver_exe : `missing — expected at ${info.receiver_exe}` },
    { key: "Tokens", value: `ingest ${info.ingest_token_seeded ? "seeded" : "not seeded"} · read ${info.read_token_seeded ? "seeded" : "not seeded"}` },
    { key: "Shipper endpoint", value: info.endpoint === "" ? "not configured" : info.endpoint },
  ];
  const deployBtn = `<button class="btn" data-lr-deploy ${info.receiver_present ? "" : "disabled"}>${info.registered ? "Repair" : "Deploy"}</button>`;
  const removeBtn = info.registered ? `<button class="btn" data-lr-remove>Remove</button>` : "";
  const flipBtn = info.endpoint_is_local
    ? ""
    : `<button class="btn" data-lr-flip ${info.registered ? "" : "disabled"}>Flip endpoint to local receiver</button>`;
  const shipperHtml = info.shipper ? logShipperInner(info.shipper) : "";
  return `<div style="font-size:12px;color:var(--text2);margin-top:16px">Log receiver<span class="info-tip" title="The log-streaming receiver runs on this machine as its own auto-start Windows service (LocalService). Ingest is on 127.0.0.1:8473; reads are served only on the machine's Tailscale address (port 8474, read-token gated). Deploy seeds its token files from the same Credential Manager ingest token the shipper uses, registers the service, and adds the firewall rule — one UAC prompt. Flipping the endpoint is separate; the shipper restarts onto it at once.">${icon("info")}</span></div>
    ${lrState}
    ${settingsRows(rows)}
    <div class="card-actions" style="margin-top:8px">${deployBtn}${removeBtn}${flipBtn}</div>
    ${shipperHtml}
    ${newToken ? `<div style="margin-top:10px">${settingsRows([{ key: "Read token (new)", value: newToken, copy: newToken, tip: "Shown once. File Muse's copy through the Secure Vault; the receiver's copy is in its data directory." }])}</div>` : ""}`;
}

/** The component-updates block of the Updates tab (updates_check_all
 *  / updates_apply): one row per wizard-managed component with the
 *  installed, latest-known and target versions, its pin, and an
 *  Apply button where the app can perform the update. Nothing here
 *  claims to be live without a check: before the first "Check now"
 *  the block says so. LiteLLM's Apply sets a newer explicit pin. */
export function componentUpdatesInner(check: ComponentUpdateCheck | null, live: boolean, task: TaskRecordWire | null): string {
  const head = `<div style="font-size:12px;color:var(--text2);margin-top:14px">Component updates<span class="info-tip" title="The components the setup wizard installs. llama.cpp updates to the build this app carries a verified checksum for (backends are stopped, the build is swapped, backends restart and must answer a smoke request — otherwise the old build is restored). LiteLLM moves only by setting a newer explicit pin. PostgreSQL and MXC are report-only.">${icon("info")}</span></div>`;
  if (!live) {
    return `${head}<div style="font-size:12px;color:var(--text2);margin-top:6px">Component update checks are available in the installed app.</div>`;
  }
  const running = task !== null && task.status === "running";
  const canApplyAll = !!check && check.components.some((c) => c.applyable && !c.pinned);
  const actions = `<div class="card-actions" style="margin-top:6px">
    <button class="btn" data-compupd-check${running ? " disabled" : ""}>${icon("search")} Check now</button>
    ${canApplyAll && !running ? `<button class="btn" data-compupd-apply-all title="Apply every available update of an unpinned component">${icon("download")} Apply all unpinned</button>` : ""}
    ${running && task ? `<button class="btn" data-compupd-abort="${esc(task.id)}">Cancel update</button>` : ""}</div>`;
  let taskLine = "";
  if (task) {
    const detail = asRecord(task.detail);
    const results = detail && Array.isArray(detail.results) ? detail.results.map(asRecord).filter((r): r is Record<string, unknown> => r !== null) : [];
    const lines = results.map((r) => `${esc(String(r.component ?? ""))} → ${esc(String(r.to ?? ""))}: ${esc(String(r.outcome ?? ""))} — ${esc(String(r.message ?? ""))}${r.rolled_back === true ? " (previous version restored)" : r.rolled_back === false ? " (ROLLBACK FAILED — see the task log)" : ""}`);
    const color = task.status === "failed" ? "var(--err)" : "var(--text2)";
    taskLine = task.status === "running"
      ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Updating… (task ${esc(task.id)}). Dependent services are stopped and restarted by the update.</div>`
      : `<div style="font-size:12px;color:${color};margin-top:6px">Last update ${esc(task.status)}${lines.length ? `:<br>${lines.join("<br>")}` : "."}</div>`;
  }
  if (!check) {
    return `${head}<div style="font-size:12px;color:var(--text2);margin-top:6px">Not checked yet in this session.</div>${actions}${taskLine}`;
  }
  const v = (s: string | null): string => (s === null ? "—" : esc(s));
  const rows = check.components.map((c) => {
    const status = c.check_error ? `<span style="color:var(--err)">check failed: ${esc(c.check_error)}</span>`
      : c.update_available ? "update available" : c.installed_version === null ? "not installed / not measured" : "up to date";
    const isLitellm = c.id === "litellm";
    const applyBtn = c.applyable && !running && c.target_version
      ? `<button class="btn" data-compupd-apply="${esc(c.id)}" data-compupd-version="${isLitellm ? esc(c.target_version) : ""}" style="padding:1px 7px;font-size:11px">${isLitellm ? `Pin ${esc(c.target_version)} &amp; apply` : `Apply ${esc(c.target_version)}`}</button>`
      : "";
    return `<div style="padding:4px 0;border-bottom:1px solid var(--border)">
      <div style="display:flex;justify-content:space-between;align-items:center;gap:16px">
        <span class="k">${esc(c.name)}${c.pinned ? ` <span style="font-size:11px;color:var(--text2)">(pinned${c.pinned_version ? ` ${esc(c.pinned_version)}` : ""})</span>` : ""}</span>
        <span style="display:flex;gap:6px;align-items:center;font-size:12px">${status}${applyBtn}</span></div>
      <div style="font-size:12px;color:var(--text2);margin-top:2px">Installed: ${v(c.installed_version)} · Latest upstream: ${v(c.latest_available)} · Apply installs: ${v(c.target_version)}</div>
      ${c.note ? `<div style="font-size:11px;color:var(--text2);margin-top:2px">${esc(c.note)}</div>` : ""}</div>`;
  }).join("");
  return `${head}<div style="font-size:12px;color:var(--text2);margin-top:4px">Checked ${esc(fmtDateTime(check.checked_at))}</div>${rows}${actions}${taskLine}`;
}

export function settingsPaint(
  cfg: Record<string, unknown>,
  policyR: { data: MxcPolicyData | null; error?: string; live: boolean },
  activityR: { data: MxcActivityData | null; error?: string; live: boolean },
  selfTestHtml: string,
  ui: SettingsUi,
  svcR: { data: ServicesInfo; error?: string; live: boolean },
  remoteHtml = "",
  appUpdateHtml = "",
  logReceiverHtml = "",
  componentUpdatesHtml = "",
): string {
  const notif = asRecord(cfg.notifications);
  const tabs: { id: SettingsTab; label: string }[] = [
    { id: "general", label: "General" },
    { id: "remote", label: "Remote access" },
    { id: "updates", label: "Updates & diagnostics" },
    ...(notif ? [{ id: "notifications" as SettingsTab, label: "Notifications" }] : []),
    { id: "mxc", label: "MXC sandbox" },
    { id: "supervision", label: "Supervision" },
  ];
  const tab = tabs.some((t) => t.id === ui.tab) ? ui.tab : "general";

  // --- General ---
  const generalRows: SettingsRow[] = [];
  const extraRows: SettingsRow[] = [];
  const dataDir = primStr(cfg.data_dir);
  if (dataDir !== null) generalRows.push({ key: "Data directory", value: dataDir, copy: dataDir });
  const configPath = primStr(cfg.config_path);
  if (configPath !== null) generalRows.push({ key: "Configuration file", value: configPath, copy: configPath });
  const tel = asRecord(cfg.telemetry);
  let telIntervalMs: number | null = null;
  if (tel) {
    const intervalKey = ["poll_interval_ms", "interval_ms", "poll_ms", "interval"].find((k) => typeof tel[k] === "number");
    if (intervalKey) telIntervalMs = tel[intervalKey] as number;
    // Raw telemetry subkeys are detail, not summary: they move to
    // the collapsed "All configuration values" expandable below.
    // retention_secs is annotated there: stored schema with no
    // consumer in this build — configuration detail, not a setting.
    for (const [k, v] of Object.entries(tel)) {
      if (k === intervalKey) continue;
      extraRows.push({ key: `Telemetry \u00b7 ${humanLabel(k)}`, value: summarizeValue(v),
        ...(k === "retention_secs" ? { tip: "Stored in the configuration schema; no component reads it in this build — the sampler's retention window is fixed. Shown as configuration detail, not as a setting." } : {}) });
    }
  }
  const winml = asRecord(cfg.winml);
  const pgCfg = asRecord(cfg.postgres);
  const pgPort = pgCfg && typeof pgCfg.port === "number" ? pgCfg.port : null;
  // postgres joins KNOWN_CFG: it has a dedicated editor below, so
  // the raw aggregate no longer duplicates it in the dump. Setup
  // completed is wizard state, not a preference — it renders as a
  // status line under the editors, not as a settings row.
  const KNOWN_CFG = new Set(["data_dir", "config_path", "telemetry", "winml", "setup_completed", "updates", "app_update", "notifications", "mxc", "postgres"]);
  for (const [k, v] of Object.entries(cfg)) {
    if (KNOWN_CFG.has(k)) continue;
    extraRows.push({ key: humanLabel(k), value: summarizeValue(v) });
  }
  // --- General editors (settings-pages sweep, Oct 19 2026): every
  // stored value on this tab carries a working control wired to its
  // real save path — telemetry_set_interval for the poll interval,
  // the generic update_config patch for WinML and the Postgres port.
  const telBucket = telIntervalMs !== null ? cadenceBucket(telIntervalMs) : null;
  const telemetryEditor = telIntervalMs !== null ? `<div style="display:flex;justify-content:space-between;align-items:center;gap:16px;padding:4px 0;border-bottom:1px solid var(--border)">
      <span class="k">Telemetry poll interval<span class="info-tip" title="How often the GPU telemetry sampler polls. Saved immediately; the running sampler's cadence is fixed at startup, so a change applies when the app restarts. The sampler buckets the stored value: 0 = off, up to 1500 ms = 1 s, above that = 5 s. The GPUs view carries the same setting as a preset select.">${icon("info")}</span></span>
      <span style="display:flex;gap:6px;align-items:center">
        <input id="setTelInterval" type="number" min="0" max="60000" step="100" value="${telIntervalMs}" style="width:100px;font-size:12px">
        <span style="font-size:12px;color:var(--text2)">ms \u00b7 sampler: ${telBucket ? telBucket.label : ""}</span>
        <button class="btn" id="setTelApply" style="padding:1px 7px;font-size:11px">Apply</button>
      </span></div>` : "";
  const winmlEditor = winml ? `<div style="padding:4px 0;border-bottom:1px solid var(--border)">
      <div style="display:flex;justify-content:space-between;align-items:center;gap:16px">
        <span class="k">Windows ML<span class="info-tip" title="The Windows ML inference server. Enabling requires a model id — set one here or through the Setup Wizard's WinML step, which also registers the server. The port is claimed only while Windows ML is enabled. Changes save through the generic config patch and are validated before they are written.">${icon("info")}</span></span>
        ${enableSwitch({ on: winml.enabled === true, name: "Windows ML", attrs: `id="setWinmlToggle"` })}
      </div>
      <div style="display:flex;gap:6px;align-items:center;margin-top:6px;flex-wrap:wrap">
        <label style="font-size:12px;color:var(--text2)">Port <input id="setWinmlPort" type="number" min="1" max="65535" value="${typeof winml.port === "number" ? winml.port : 8090}" style="width:90px;font-size:12px"></label>
        <label style="font-size:12px;color:var(--text2)">Model <input id="setWinmlModel" type="text" value="${esc(primStr(winml.model_id) ?? "")}" placeholder="model id" style="width:180px;font-size:12px"></label>
        <button class="btn" id="setWinmlApply" style="padding:1px 7px;font-size:11px">Apply</button>
        <button class="btn" id="setWinmlRecheck" style="padding:1px 7px;font-size:11px">Re-check capabilities</button>
        <span class="info-tip" title="Probes the Windows ML server now and reports what it serves and its limits. With a model registered, the registration's recorded limits are refreshed from the probe.">${icon("info")}</span>
      </div></div>` : "";
  const postgresEditor = pgPort !== null ? `<div style="display:flex;justify-content:space-between;align-items:center;gap:16px;padding:4px 0;border-bottom:1px solid var(--border)">
      <span class="k">PostgreSQL port<span class="info-tip" title="Loopback port of the app-managed PostgreSQL server the gateway migrates against at launch. Saved immediately and validated against every other claimed port; it applies the next time PostgreSQL starts — a running gateway keeps the server it started with.">${icon("info")}</span></span>
      <span style="display:flex;gap:6px;align-items:center">
        <input id="setPgPort" type="number" min="1" max="65535" value="${pgPort}" style="width:100px;font-size:12px">
        <button class="btn" id="setPgApply" style="padding:1px 7px;font-size:11px">Apply</button>
      </span></div>` : "";
  const setupStatus = primStr(cfg.setup_completed) !== null
    ? `<div style="font-size:12px;color:var(--text2);margin-top:10px">Setup: ${cfg.setup_completed ? "completed" : "not completed"}<span class="info-tip" title="Wizard completion state recorded by the Setup Wizard — not a preference. Run the Setup Wizard to change it.">${icon("info")}</span></div>`
    : "";
  const generalPanel = `${settingsRows(generalRows)}
    ${telemetryEditor}${winmlEditor}${postgresEditor}${setupStatus}
    ${extraRows.length ? `<div style="margin-top:12px"><button class="btn" data-all-toggle aria-expanded="${ui.allOpen}" title="All configuration values" aria-label="All configuration values" style="padding:2px 8px;font-size:11px">${icon(ui.allOpen ? "chevD" : "chevR")}</button>
      ${ui.allOpen ? `<div style="margin-top:8px">${settingsRows(extraRows)}</div>` : ""}</div>` : ""}`;

  // --- Updates & diagnostics ---
  const updates = asRecord(cfg.updates);
  let policyBlock = "";
  if (updates) {
    const { components, ...restUpdates } = updates;
    const comps = asRecord(components);
    const compHtml = comps ? Object.entries(comps).map(([name, v]) => {
      const c = asRecord(v);
      if (!c) return `<div style="display:flex;justify-content:space-between;gap:16px;padding:4px 0"><span class="k">${esc(name)}</span><span style="font-size:12px">${esc(summarizeValue(v))}</span></div>`;
      const pinned = primStr(c.pinned_version);
      const checked = primStr(c.last_checked);
      // Pin and auto-update carry working controls wired to their
      // dedicated ops (updates_pin / updates_set_auto); last_checked
      // stays text — it is a recorded timestamp, not a setting.
      return `<div style="padding:4px 0;border-bottom:1px solid var(--border)">
        <div style="display:flex;justify-content:space-between;align-items:center;gap:16px">
          <span class="k">${esc(name)}</span>
          <span style="display:flex;gap:6px;align-items:center">
            <span style="font-size:12px;color:var(--text2)">Auto-update</span>
            ${enableSwitch({ on: c.auto_update === true, name: `${name} auto-update`, attrs: `data-comp-auto="${esc(name)}"` })}
          </span></div>
        <div style="display:flex;gap:6px;align-items:center;margin-top:6px;flex-wrap:wrap">
          <label style="font-size:12px;color:var(--text2)">Pinned version <input type="text" data-comp-pin-input="${esc(name)}" value="${pinned !== null ? esc(pinned) : ""}" placeholder="unpinned" style="width:170px;font-size:12px"></label>
          <button class="btn" data-comp-pin-save="${esc(name)}" style="padding:1px 7px;font-size:11px">${pinned !== null ? "Save pin" : "Pin"}</button>
          ${pinned !== null ? `<button class="btn" data-comp-unpin="${esc(name)}" style="padding:1px 7px;font-size:11px">Unpin</button>` : ""}
          ${checked !== null ? `<span style="font-size:12px;color:var(--text2)">Checked: ${esc(fmtDate(checked))}</span>` : ""}
        </div></div>`;
    }).join("") : "";
    const restRows = humanEntries(restUpdates);
    // No policy content at all -> the policy block is omitted
    // entirely; the diagnostics actions below remain (they are the
    // tab's content, not an empty state).
    policyBlock = `${restRows.length ? settingsRows(restRows) : ""}
      ${compHtml}`;
  }
  const updatesPanel = `${appUpdateHtml}
    ${componentUpdatesHtml}
    ${policyBlock ? `<div style="font-size:12px;color:var(--text2);margin-top:14px">Component pins<span class="info-tip" title="Per-component version pins the setup wizard manages (LiteLLM, llama.cpp). Separate from LLMdil's own updates above.">${icon("info")}</span></div>${policyBlock}` : ""}
    <div style="font-size:12px;color:var(--text2);margin-top:14px">Diagnostics</div>
    <div class="card-actions"><a class="btn" href="#/wizard">${icon("search")} Open Setup Wizard</a>
    <button class="btn icon-only" id="setExportBtn" title="Export diagnostics bundle" aria-label="Export diagnostics bundle">${icon("download")}</button><span class="info-tip" title="The bundle contains raw, unredacted process logs. Review it before sharing.">${icon("info")}</span>
    <button class="btn icon-only" id="setOpenFolder" title="Open folder" aria-label="Open folder" style="display:none">${icon("folder")}</button></div>`;

  // --- Notifications: master switch + one toggle per category,
  // each persisted through notifications_set_config. A missing key
  // reads as on (the Rust serde defaults agree), so a config written
  // before the category switches existed renders its true state.
  // Category toggles disable while the master is off (they keep
  // their stored value; the backend ANDs them with the master). ---
  const notifOn = (k: string): boolean => notif?.[k] !== false;
  const notifMaster = notifOn("enabled");
  const notifRow = (key: string, label: string, desc: string): string => {
    const on = notifOn(key);
    const dis = key !== "enabled" && !notifMaster;
    return `<div style="display:flex;justify-content:space-between;align-items:center;gap:16px;padding:4px 0;border-bottom:1px solid var(--border)">
      <span class="k">${esc(label)}<span class="info-tip" title="${esc(desc)}">${icon("info")}</span></span>
      ${enableSwitch({ on, name: key === "enabled" ? "Notifications" : `${label} notifications`, attrs: `data-notif-toggle="${key}"`, disabled: dis })}</div>`;
  };
  const notifPanel = notif ? `${notifRow("enabled", "Notifications", "Master switch for Windows toast notifications. Off silences every category below.")}
    ${notifRow("operations", "Operations", "Long-running work finishing: a setup step completing, a model download completing.")}
    ${notifRow("failures", "Failures", "A backend stopping unexpectedly, the gateway going down or failing to start, a setup step failing, a model download failing.")}
    ${notifRow("activation", "Activation", "An activation plan finishing applying, or failing to apply.")}
    <div class="card-actions" style="margin-top:10px"><button class="btn" data-notif-test>${icon("bell")} Send test notification</button></div>
    <div style="font-size:12px;color:var(--text2);margin-top:6px">The test fires immediately, even with the master switch off — it verifies Windows can show this app's toasts.</div>` : "";

  // --- MXC sandbox: summary first — Mode control, Active policy,
  // Recent activity, self-test. The button group is the ONE home
  // for the mode (no Mode row); each button's definition is its
  // tooltip; the methodology is the Mode label's info-tip. ---
  const mxc = asRecord(cfg.mxc);
  const mode = (mxc ? primStr(mxc.mode) : null) ?? "";
  const modeLc = mode.toLowerCase();
  const policyFile = mxc ? primStr(mxc.path) : null;
  const modeBlock = `<div style="font-size:12px;color:var(--text2)">Mode<span class="info-tip" title="The mode is saved configuration: this app records it, and the MXC sandbox runtime applies it when that runtime executes tools.">${icon("info")}</span></div>
    <div class="card-actions" style="margin-top:6px">${MXC_MODES.map((m) =>
      `<button class="btn${modeLc === m.mode ? " primary" : ""}" data-mxc-mode="${m.mode}" title="${esc(m.def)}">${m.label}</button>`).join("")}</div>`;
  const selfTest = `<div style="font-size:12px;color:var(--text2);margin-top:14px">Self-test</div>
    <div class="card-actions"><button class="btn icon-only" data-mxc-selftest title="Run self-test" aria-label="Run self-test">${icon("shield")}</button></div>
    <div id="mxcSelfTest">${selfTestHtml}</div>`;
  // The policy file path is stored config (mxc.path) — it gets an
  // editor wired to the generic config patch. The policy document's
  // contents (version, default, rules) stay read-only below: they
  // are the file's data, edited in the file itself.
  const policyFileEditor = policyFile !== null ? `<div style="display:flex;gap:6px;align-items:center;margin-top:12px;flex-wrap:wrap">
      <label style="font-size:12px;color:var(--text2)">Policy file <input id="setMxcPath" type="text" value="${esc(policyFile)}" style="width:340px;font-size:12px"></label>
      <button class="btn" id="setMxcPathApply" style="padding:1px 7px;font-size:11px">Apply</button>
      <span class="info-tip" title="Path of the MXC policy document, saved to the configuration. The app reads the policy when it starts, so a path change applies on restart. If the file does not exist, the built-in default policy applies.">${icon("info")}</span>
    </div>` : "";
  const mxcPanel = `${modeBlock}${policyFileEditor}${mxcPolicyInner(policyR, ui.rulesFilter, ui.rulesOpen, null)}${mxcActivityInner(activityR)}${selfTest}`;

  const supervisionPanel = `<div id="supSvc">${supervisionInner(svcR)}</div><div id="supLr">${logReceiverHtml}</div>`;
  const panel = tab === "general" ? generalPanel
    : tab === "remote" ? remoteHtml
    : tab === "updates" ? updatesPanel
    : tab === "notifications" ? notifPanel
    : tab === "supervision" ? supervisionPanel
    : mxcPanel;
  return `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("gear")} Settings</div>
    <div class="viewsel" role="tablist" style="margin-bottom:12px">${tabs.map((t) => `<button data-set-tab="${t.id}"${t.id === tab ? ` class="active"` : ""}>${t.label}</button>`).join("")}</div>
    ${panel}</div>`;
}


/** Certificate lifecycle notes under the Regenerate button (SEC-08):
 *  why the shell generated a new certificate on its own, and the
 *  informational SAN drift. Drift never regenerates by itself —
 *  paired devices pin the fingerprint, not the addresses — so it is
 *  only a hint that Regenerate (which forces a re-pair) would pick up
 *  the new addresses. Renders nothing when there is nothing to say. */
function certNotes(status: AdminApiStatus): string {
  const notes: string[] = [];
  const why: Record<string, string> = {
    generated_missing: "A new certificate was generated because none was stored.",
    regenerated_key_mismatch: "The certificate was regenerated because the stored key did not match it. Paired devices must pair again.",
    regenerated_expiring: "The certificate was regenerated because it had expired or was about to. Paired devices must pair again.",
    regenerated_corrupt: "The certificate was regenerated because the stored files were unreadable. Paired devices must pair again.",
  };
  const reason = status.cert_source ? why[status.cert_source] : undefined;
  if (reason) {
    notes.push(`<div style="font-size:12px;color:var(--warn);margin-top:6px">${esc(reason)}${status.cert_detail ? ` (${esc(status.cert_detail)})` : ""}</div>`);
  }
  if (status.san_drift && status.san_drift_ips && status.san_drift_ips.length) {
    notes.push(`<div style="font-size:12px;color:var(--text2);margin-top:6px">Not in the certificate: ${status.san_drift_ips.map(esc).join(", ")}<span class="info-tip" title="This machine has addresses the certificate does not list (for example after a DHCP change or Tailscale coming up). Paired devices pin the certificate fingerprint, not the address, so they keep working. Regenerate only if a client needs the new address in the certificate; every paired device must then pair again.">${icon("info")}</span></div>`);
  }
  return notes.join("");
}

/** The pairing QR (PLAN.md §3.8): the shell renders it as an inline
 *  <svg> of black/white modules from exactly the values listed below
 *  it (the CSP admits no data: images, so it is inlined). Accepted
 *  only as a bare <svg> element with nothing scriptable in it;
 *  anything else renders nothing and the text values still pair. */
function pairingQrHtml(svg: string | null | undefined): string {
  if (!svg) return "";
  const s = svg.trim();
  const lower = s.toLowerCase();
  if (!lower.startsWith("<svg") || !lower.endsWith("</svg>")) return "";
  if (lower.includes("<script") || lower.includes("<foreignobject") || /\son[a-z]+\s*=/.test(lower) || lower.includes("javascript:")) return "";
  return `<div class="ra-qr" role="img" aria-label="Pairing QR code" style="display:inline-block;margin:8px 0;background:#fff;line-height:0">${s}</div>`;
}

/** Remote Access tab: the admin API's state, its users, and
 *  device pairing. Values are Label: Value rows with copy buttons
 *  (the global [data-copy-value] handler serves them); state the
 *  API has not produced yet renders nothing. One-time values (a
 *  temporary password, an enrollment payload) show until the next
 *  action replaces them. */
export function remoteAccessHtml(
  status: AdminApiStatus | null,
  users: AdminApiUser[],
  temp: { user: string; password: string } | null,
  enroll: (AdminApiPairing & { user: string }) | null,
  audit: AdminApiAuditEntry[] = [],
): string {
  if (!status) return loadingHtml("Loading remote access\u2026");
  const stateText = status.running
    ? `<span style="color:var(--ok)">Running</span>`
    : status.enabled
      ? `<span style="color:var(--err)">Enabled \u2014 not running</span>`
      : `<span style="color:var(--text2)">Disabled</span>`;
  const fwText = status.firewall_needed === false ? "Not needed (loopback only)"
    : status.firewall === "ok" ? "Allowed: local subnet + Tailscale"
    : status.firewall === "missing" ? "Rule missing"
    : status.firewall === "drifted" ? "Rule differs from expected"
    : status.firewall === "unsupported" ? "Not reported on this platform"
    : "Unknown";
  // Bind choice (auth-plan §5). Options: loopback, all interfaces,
  // or one of this machine's current addresses; a configured address
  // that is no longer present stays listed (marked) so the saved
  // choice is visible rather than silently replaced.
  const bindMode = status.bind_mode ?? "all";
  const bindIp = status.bind_ip ?? null;
  const candidates = [...(status.bind_candidates ?? [])];
  if (bindIp && !candidates.includes(bindIp)) candidates.push(bindIp);
  const bindSel = bindMode === "interface" && bindIp ? `ip:${bindIp}` : bindMode;
  const bindOptions = [
    { value: "loopback", label: "This computer only (127.0.0.1)" },
    { value: "all", label: "All interfaces (LAN + Tailscale)" },
    ...candidates.map((ip) => ({
      value: `ip:${ip}`,
      label: `Interface ${ip}${(status.bind_candidates ?? []).includes(ip) ? "" : " (not present)"}`,
    })),
  ];
  const bindEditor = `<div style="display:flex;justify-content:space-between;align-items:center;gap:16px;padding:4px 0;border-bottom:1px solid var(--border)">
      <span class="k">Listen on<span class="info-tip" title="Which addresses the Admin API listens on. This computer only: reachable from this machine alone, no firewall rule. All interfaces: the LAN and Tailscale, limited by the firewall rule to the local subnet and the Tailscale range. Interface: one address of this machine, under the same rule. If that address is missing when the API starts, it listens on this computer only and says why. Saved through the generic config patch; applies when the API is switched off and on, or the app restarts.">${icon("info")}</span></span>
      <span style="display:flex;gap:6px;align-items:center">
        <select id="raBind" style="font-size:12px">${bindOptions.map((o) => `<option value="${esc(o.value)}"${o.value === bindSel ? " selected" : ""}>${esc(o.label)}</option>`).join("")}</select>
        <button class="btn" data-ra-bind-save style="padding:1px 7px;font-size:11px">Apply</button>
      </span></div>`;
  const rows: SettingsRow[] = [
    { key: "Bind", value: status.bind },
    { key: "Firewall rule", value: fwText },
    { key: "Users", value: String(status.user_count) },
  ];
  if (status.base_url) rows.unshift({ key: "Base URL", value: status.base_url, copy: status.base_url });
  if (status.cert_sha256) rows.push({ key: "Certificate SHA-256", value: status.cert_sha256, copy: status.cert_sha256 });
  const roles = ["viewer", "operator", "configurator", "admin"];
  const userRows = users.map((u) => `<div style="display:flex;justify-content:space-between;gap:16px;align-items:center;padding:3px 0">
    <span style="font-size:12px">${esc(u.username)}${u.disabled ? ` <span style="color:var(--text2)">(disabled)</span>` : ""}${u.must_change_password ? ` <span style="color:var(--warn)">(must change password)</span>` : ""}</span>
    <span style="display:flex;gap:6px;align-items:center">
      <select data-ra-role-for="${esc(u.id)}" style="font-size:12px">${roles.map((r) => `<option value="${r}"${r === u.role ? " selected" : ""}>${r}</option>`).join("")}</select>
      <button class="btn" data-ra-apply-role="${esc(u.id)}" style="padding:1px 7px;font-size:11px">Apply</button>
      ${enableSwitch({ on: !u.disabled, name: `User ${u.username}`, attrs: `data-ra-toggle-disabled="${esc(u.id)}"` })}
      <button class="btn" data-ra-reset="${esc(u.id)}" data-ra-name="${esc(u.username)}" style="padding:1px 7px;font-size:11px">Reset password</button>
      <button class="btn" data-ra-enroll="${esc(u.id)}" data-ra-name="${esc(u.username)}" style="padding:1px 7px;font-size:11px">Pairing code</button>
      <button class="btn" data-ra-delete="${esc(u.id)}" data-ra-name="${esc(u.username)}" style="padding:1px 7px;font-size:11px">Delete</button>
    </span></div>`).join("");
  return `<div style="font-size:12px;color:var(--text2)">Admin API<span class="info-tip" title="Serves the LLMdil control API to devices on this network. Access requires a user account and is limited by the firewall rule to the local subnet and Tailscale.">${icon("info")}</span></div>
    <div style="display:flex;justify-content:space-between;gap:16px;margin:6px 0"><span class="k">Status</span><span class="v" style="font-size:12px">${stateText}</span></div>
    <div style="display:flex;justify-content:space-between;align-items:center;gap:16px;padding:4px 0;border-bottom:1px solid var(--border)">
      <span class="k">Port<span class="info-tip" title="TCP port for the Admin API listener, saved through the generic config patch. The running listener keeps its current port until the API is switched off and on, or the app restarts; the firewall rule reconciles to the saved port when the API next starts.">${icon("info")}</span></span>
      <span style="display:flex;gap:6px;align-items:center">
        <input id="raPort" type="number" min="1" max="65535" value="${status.port}" style="width:100px;font-size:12px">
        <button class="btn" data-ra-port-save style="padding:1px 7px;font-size:11px">Apply</button>
      </span></div>
    ${bindEditor}
    ${settingsRows(rows)}
    ${status.bind_fallback ? `<div style="font-size:12px;color:var(--warn);margin-top:6px">${esc(status.bind_fallback)}</div>` : ""}
    ${status.note ? `<div style="font-size:12px;color:var(--err);margin-top:6px">${esc(status.note)}</div>` : ""}
    <div class="card-actions" style="margin-top:10px">${enableSwitch({ on: status.enabled, name: "Admin API", attrs: "data-ra-toggle" })}
    <button class="btn" data-ra-regen>Regenerate certificate</button></div>
    ${certNotes(status)}
    <div style="font-size:12px;color:var(--text2);margin-top:16px">${users.length === 0 ? "First administrator" : "Users"}<span class="info-tip" title="Roles: viewer reads status; operator runs backends and the gateway; configurator also edits configuration; admin manages users and the API itself. Passwords need at least 12 characters.">${icon("info")}</span></div>
    ${userRows}
    <div style="display:flex;gap:6px;align-items:center;margin-top:8px;flex-wrap:wrap">
      <input id="raUser" type="text" placeholder="Username" style="font-size:12px" />
      <input id="raPass" type="password" placeholder="Password (12+ characters)" style="font-size:12px" />
      <select id="raRole" style="font-size:12px">${roles.map((r) => `<option value="${r}"${r === "admin" && users.length === 0 ? " selected" : ""}>${r}</option>`).join("")}</select>
      <button class="btn" data-ra-create style="padding:1px 7px;font-size:11px">Create user</button>
    </div>
    ${audit.length ? `<div style="font-size:12px;color:var(--text2);margin-top:16px">Audit log<span class="info-tip" title="Every API mutation and sign-in event, newest first. Full log: logs/admin-api-audit.jsonl in the data directory.">${icon("info")}</span></div>
    <div style="font-size:12px">${audit.map((e) => `<div style="display:flex;justify-content:space-between;gap:16px;padding:2px 0"><span>${esc(new Date(e.ts_ms).toLocaleString())} \u00b7 ${esc(e.actor)} \u00b7 ${esc(e.action)}${e.target ? ` \u00b7 ${esc(e.target)}` : ""}</span><span style="color:${e.result.startsWith("2") ? "var(--ok)" : "var(--err)"}">${esc(e.result)}</span></div>`).join("")}</div>` : ""}
    ${temp ? `<div style="margin-top:12px">${settingsRows([{ key: `Temporary password \u00b7 ${temp.user}`, value: temp.password, copy: temp.password, tip: "Shown once. The user must change it at first sign-in." }])}</div>` : ""}
    ${enroll ? `<div style="margin-top:12px"><div style="font-size:12px;color:var(--text2)">Pairing \u00b7 ${esc(enroll.user)}<span class="info-tip" title="Scan the code with the LLMdil app, or enter these values in it, to pair this device. The code is single-use and expires in 10 minutes.">${icon("info")}</span></div>${pairingQrHtml(enroll.qr_svg)}${settingsRows([
      { key: "Base URL", value: enroll.base_url, copy: enroll.base_url },
      { key: "Certificate SHA-256", value: enroll.cert_sha256 ?? "", copy: enroll.cert_sha256 ?? undefined },
      { key: "Enrollment code", value: enroll.enrollment_code ?? "", copy: enroll.enrollment_code ?? undefined },
    ])}</div>` : ""}`;
}

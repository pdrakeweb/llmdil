/* Backends view: the backend card and the view shell (split from
   static.ts). */

import type { BackendAutoRestart, BackendRow, CacheSelectionLike, LogRow, ModelRow } from "../api";
import { fmtCount, fmtGb, trim1 } from "../charts";
import { esc } from "../html";
import { icon } from "../icons";
import { LIFECYCLE_STATES, META, applyBlockedReason, fromWire, lifecycleActions, lifecycleChip, lifecycleDot, noteOfferedActions } from "../lifecycle";
import type { LifecycleState } from "../lifecycle";
import { enableSwitch } from "../switch";
import { pageState } from "../viewState";
import { MOE_CACHE_DEFAULT_MIB, MOE_CACHE_PRESETS_MIB, MTP_POSITIONS, fitChipHtml, flagsCacheHtml, flagsCtxBatchHtml, flagsParallelHtml, flagsRatedLineHtml, flagsRawWarningsHtml, moeCacheSavedTokenText, moeCacheTokenText, mtpConflictText, mtpPosition, mtpSavedTokenText, mtpTokenText, offloadIsUntested, offloadSliderHtml, predictedCardTip, sameCacheSelection } from "./backendFlags";
import type { FlagsDraft, PlaceCardState } from "./backendFlags";
import { SELECT_STYLE, fmtElapsed, fmtTime, fmtUptime, gpuShortName, loadingHtml, modelFileShortName, modelOptions, modelShortName, quoteArg, sameModelFile, topbar } from "./common";
import type { CardNote } from "./common";

/** One backend's measured VRAM on one GPU (mib null = not measured). */
export interface ResidencyRow { gpu: number; name: string; mib: number | null }

/** The GPU indices a backend's saved split places it on. */
export function configuredGpus(b: Pick<BackendRow, "tensor_split">): number[] {
  return (b.tensor_split ?? []).map((f, i) => (f > 0 ? i : -1)).filter((i) => i >= 0);
}

/** The overview list's GPUs column: every configured GPU, each saying
 *  what is measured there. A loaded backend's GPU without a figure reads
 *  "not measured", never as absent; a stopped backend lists where it
 *  loads at its next start. */
export function gpuColumnText(b: Pick<BackendRow, "tensor_split">, active: boolean, res: readonly ResidencyRow[]): string {
  const configured = configuredGpus(b);
  const measured = res.filter((r) => r.mib !== null && r.mib >= 1024).map((r) => r.gpu);
  const unmeasured = res.filter((r) => r.mib === null).map((r) => r.gpu);
  if (!active && !res.length) return configured.length ? configured.map((i) => `GPU ${i}`).join(", ") : "\u2014";
  const all = [...new Set([...configured, ...measured, ...unmeasured])].sort((a, z) => a - z);
  if (!all.length) return res.length ? "context only" : "\u2014";
  return all.map((i) => {
    if (measured.includes(i)) return `GPU ${i}`;
    return res.some((r) => r.gpu === i && r.mib !== null) ? `GPU ${i} (context only)` : `GPU ${i} (not measured)`;
  }).join(", ");
}

/** The crash-restart schedule as Label: Value text. */
export function autoRestartText(a: BackendAutoRestart, now: number = Date.now()): string {
  if (a.gave_up) return `Auto-restart: gave up after ${a.attempt} of ${a.max} attempts`;
  const secs = a.next_at_ms !== null ? Math.max(0, Math.ceil((a.next_at_ms - now) / 1000)) : null;
  return `Auto-restart: attempt ${a.attempt} of ${a.max}${secs === null ? "" : secs > 0 ? ` in ${fmtElapsed(secs)}` : " now"}`;
}

/** The context length raw flags set themselves, or null. */
export function rawContext(raw: string): number | null {
  const m = [...raw.matchAll(/(?:^|\s)(?:-c|--ctx-size)(?:\s+|=)(\d+)(?=\s|$)/g)].pop();
  return m ? Number(m[1]) : null;
}

/** What is free for this backend across its placed GPUs, against what
 *  it needs there: the payload's own figures, else telemetry (free VRAM
 *  plus what the backend itself already holds). Null = unknown. */
export function fitNow(
  b: BackendRow,
  gpuFreeMiB: Map<number, number>,
  res: readonly ResidencyRow[],
): { fitsNow: boolean | null; neededMib: number | null; freeMib: number | null; gpu: number | null } {
  const rows = b.fit?.per_gpu ?? [];
  let need = 0;
  let free = 0;
  let known = rows.length > 0;
  let fits = true;
  // The figures name the GPU that is short: sums over the split can
  // read as enough while one GPU is not.
  let short: { gpu: number; need: number; free: number } | null = null;
  for (const r of rows) {
    const held = res.find((x) => x.gpu === r.gpu);
    const f = r.free_mib ?? (gpuFreeMiB.has(r.gpu) && held?.mib !== null ? (gpuFreeMiB.get(r.gpu) ?? 0) + (held?.mib ?? 0) : null);
    need += r.required_mib;
    if (f === null) { known = false; continue; }
    free += f;
    if (!(r.fits_free ?? r.required_mib <= f)) {
      fits = false;
      short ??= { gpu: r.gpu, need: r.required_mib, free: f };
    }
  }
  return {
    fitsNow: typeof b.fits_now === "boolean" ? b.fits_now : known ? fits : null,
    neededMib: short ? short.need : rows.length ? need : null,
    freeMib: short ? short.free : known ? free : null,
    gpu: short ? short.gpu : null,
  };
}

export function backendCard(
  b: BackendRow,
  openArgv: Set<string>,
  openMenus: Set<string>,
  argvCache: Map<string, string[]>,
  msgs: Map<string, CardNote>,
  logTails: Map<string, LogRow[]>,
  inFlight: Set<string>,
  models: ModelRow[],
  confirmRemove: Set<string>,
  residency: Map<string, ResidencyRow[]>,
  gpuNames: Map<number, string>,
  flags: FlagsDraft | null,
  predictedHtml: string,
  place: PlaceCardState | null,
  gpuTotalsMiB: Map<number, number>,
  gpuFreeMiB: Map<number, number>,
  detailsOpen: boolean,
  predictRefreshing: Map<string, number>,
  confirmOverride: Set<string>,
  armedWddmStart: Set<string>,
): string {
  // The row's state is the EFFECTIVE lifecycle state (wire state
  // with the shared store's pending overlay, normalized by the
  // view's load): a restart from any surface reads "restarting…".
  const st: LifecycleState = fromWire(b.lifecycle ?? b.state);
  const active = META[st].isLive || META[st].isTransition;
  // Buttons render disabled while this card's (non-lifecycle) action
  // is in flight. The set lives outside the repainted DOM, so the
  // disabled state survives the repaints a poll or a log-tail fetch
  // triggers. Lifecycle buttons follow the shared rule instead
  // (lifecycleActions: disabled with the reason mid-transition).
  const dis = inFlight.has(b.id) ? " disabled" : "";
  const applyBlocked = applyBlockedReason({ kind: "backend", id: b.id });
  const lcSpecs = lifecycleActions(st, {
    kind: "backend", enabled: b.enabled, configured: b.configured,
    ...(applyBlocked ? { blockedReason: applyBlocked } : {}),
  });
  noteOfferedActions(`backend:${b.id}`, lcSpecs);
  const lcSpec = (a: "start" | "stop" | "restart") => lcSpecs.find((x) => x.action === a);
  const lcAttrs = (a: "start" | "stop" | "restart"): string => {
    const sp = lcSpec(a);
    return `${inFlight.has(b.id) || sp?.disabled ? " disabled" : ""}${sp?.reason ? ` title="${esc(sp.reason)}"` : ""}`;
  };
  const stopBtn = `<button class="btn" data-back-act="stop" data-back-id="${esc(b.id)}"${lcAttrs("stop")}>${icon("pause")} Stop</button>`;
  const restartBtn = `<button class="btn${b.fit?.state === "wddm_paged" && armedWddmStart.has(b.id) ? " armed" : ""}" data-back-act="restart" data-back-id="${esc(b.id)}"${lcAttrs("restart")}>${icon("restart")} Restart</button>`;
  // A WDDM paged fit starts only through the two-click confirm
  // (the delete-confirm idiom): the first click arms the button,
  // the second starts with the overcommit flag. The armed label
  // states the paged amount and nothing else.
  const wddmPaged = b.fit?.state === "wddm_paged";
  const startBtn = wddmPaged
    ? `<button class="btn${armedWddmStart.has(b.id) ? " armed" : ""}" data-back-act="start" data-back-id="${esc(b.id)}" title="${armedWddmStart.has(b.id) ? "Confirm start with paging" : "Start (pages to system RAM)"}"${inFlight.has(b.id) || lcSpec("start")?.disabled ? " disabled" : ""}>${icon("play")} ${armedWddmStart.has(b.id) ? `Start — ${fmtGb((b.fit?.paged_mib ?? 0) / 1024)} GiB paged` : "Start"}</button>`
    : `<button class="btn" data-back-act="start" data-back-id="${esc(b.id)}"${lcAttrs("start")}>${icon("play")} Start</button>`;
  const testBtn = `<button class="btn icon-only" data-back-act="test" data-back-id="${esc(b.id)}" title="Test backend" aria-label="Test ${esc(b.id)}"${dis}>${icon("bolt")}</button>`;
  // The tool-call probe (MCP design §7.2) and the go-to-logs link
  // (tracker #47) are NOT card-face buttons: the wrench read as an
  // edit control (Pete's report, 2026-10-20), so both live in the
  // card's Other-actions menu below. Every backend has a Logs-view
  // scope (its supervisor ring, retained even after removal), so
  // the menu's logs item is unconditional, like the button was.
  // Enable/disable is the shared state switch (rule 16). It renders
  // for every configured backend so the state is always visible;
  // the control is inert exactly when the old buttons were not
  // offered — an action in flight, or enabled in a live state (the
  // command layer refuses disable-while-running: a running backend
  // is stopped, not disabled). The data-back-act flips with the
  // state, so the delegated handler fires the same
  // applyBackendConfig call the two buttons fired.
  const toggleable = !b.enabled || st === "stopped" || st === "failed";
  const enableSw = enableSwitch({
    on: b.enabled,
    name: `Backend ${b.id}`,
    attrs: `data-back-act="${b.enabled ? "disable" : "enable"}" data-back-id="${esc(b.id)}"`,
    title: b.enabled && !toggleable ? `Backend ${b.id}: enabled (stop it before disabling)` : undefined,
    disabled: inFlight.has(b.id) || !toggleable,
  });
  const removeBtn = `<button class="btn icon-only danger${confirmRemove.has(b.id) ? " armed" : ""}" data-back-act="remove" data-back-id="${esc(b.id)}" title="${confirmRemove.has(b.id) ? "Confirm remove" : "Remove backend"}" aria-label="${confirmRemove.has(b.id) ? "Confirm remove" : "Remove"} ${esc(b.id)}"${dis}>${icon("trash")}</button>`;
  // The shared lifecycle rule (lifecycleActions) decides Start /
  // Stop / Restart and their disabled reasons — the same rule as the
  // Dashboard cards and Settings > Supervision:
  // - Stop while live, whether the backend is configured, enabled, or
  //   an orphan — stopping is always allowed (disabled, with the
  //   reason, mid-restart/stop and when nothing runs).
  // - Restart for configured live backends (disabled with the reason
  //   when the backend is disabled — the shared rule).
  // - Start for stopped/failed configured backends (disabled with the
  //   reason when the backend is disabled).
  // - The enable/disable switch shows on every configured backend;
  //   it is toggleable only in the states listed for it above.
  // - Test only while running.
  const parts: string[] = [];
  // Fixed slots: Start or Restart first, Stop second.
  if (b.configured && lcSpec("restart")) parts.push(restartBtn);
  if (b.configured && lcSpec("start")) parts.push(startBtn);
  if (lcSpec("stop") && (b.configured || !lcSpec("stop")?.disabled)) parts.push(stopBtn);
  if (b.configured) parts.push(enableSw);
  if (st === "running") parts.push(testBtn);
  // Removal is refused by the command layer while running; offer it
  // only for configured backends with no live process.
  if (b.configured && !active) parts.push(removeBtn);
  const actions = parts.join("\n        ");
  // Advanced, infrequently used options live behind one hamburger
  // menu so the card's primary action row stays within the card.
  // Menu-open state lives outside the repainted DOM, like the other
  // card state, so a poll repaint cannot close an open menu.
  const menuOpen = openMenus.has(b.id);
  const advancedMenu = `<div class="advanced-menu-wrap" data-back-menu-wrap>
      <button class="btn icon-only" data-back-menu="${esc(b.id)}" aria-haspopup="menu" aria-expanded="${menuOpen}" title="Advanced options" aria-label="Advanced options for ${esc(b.id)}"${dis}>${icon("menu")}</button>
      ${menuOpen ? `<div class="advanced-menu" role="menu" aria-label="Advanced options for ${esc(b.id)}">
        ${b.configured ? `<button class="menu-item" role="menuitem" data-back-menu-item data-back-act="flags" data-back-id="${esc(b.id)}"${dis}>${icon("gear")} Edit</button>` : ""}
        <button class="menu-item" role="menuitem" data-back-menu-item data-back-act="argv" data-back-id="${esc(b.id)}"${dis}>${icon("logs")} Launch command</button>
        ${b.configured ? `<button class="menu-item" role="menuitem" data-back-menu-item data-back-act="toolcall" data-back-id="${esc(b.id)}"${dis}>${icon("wrench")} Test tool calls</button>` : ""}
        ${b.configured ? `<button class="menu-item" role="menuitem" data-back-menu-item data-back-act="tuning" data-back-id="${esc(b.id)}">${icon("gauge")} Optimize this backend</button>` : ""}
        <a class="menu-item" role="menuitem" data-back-menu-item data-back-id="${esc(b.id)}" href="#/logs?scope=${esc(encodeURIComponent(b.id))}&follow=1">${icon("logs")} View all logs</a>
      </div>` : ""}
    </div>`;
  const argvOpen = openArgv.has(b.id);
  const argv = argvOpen
    ? `<div style="margin-top:10px;font-size:11px;color:var(--text3)">Launch command (next start)</div>
      <pre style="margin-top:4px;padding:10px 12px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-all">${esc((argvCache.get(b.id) ?? []).map(quoteArg).join(" "))}</pre>`
    : "";
  const note = msgs.get(b.id);
  const msgHtml = note
    ? `<span style="color:${note.err ? "var(--err)" : "var(--text2)"}">${esc(note.text)}</span>` : "";
  // Card narrative lives in header-chip tooltips, never as inline
  // prose (Pete's Label: Value rule, 2026-10-16): the orphan and
  // adopted explanations ride the title attribute of the chips that
  // already flag those states. The orphan text follows the process
  // state — a failed or stopping orphan is not "still running".
  const orphanText = !b.configured
    ? b.state === "failed"
      ? "It failed, and its entry was removed from the configuration. It cannot be restarted."
      : active
        ? "Still running, but its entry was removed from the configuration. It can be stopped here; it cannot be restarted."
        : "Its entry was removed from the configuration. It cannot be restarted."
    : "";
  // An adopted backend was started by a previous launch of the app
  // (its processes survive the app closing) and taken over by pid:
  // it can be stopped/restarted here, but its earlier output is not
  // in this session's log ring.
  const adoptedText = b.adopted
    ? "Started by a previous launch and adopted by this session. Output from before adoption is not in the Logs view."
    : "";
  // One rendering for a stored log line, shared by the failed
  // block below and the Details section's latest-lines block.
  const logLineHtml = (r: LogRow): string => `<div class="ln"><span class="t">${esc(fmtTime(r.ts))}</span> <span class="chip">${esc(r.stream)}</span> ${esc(r.line)}</div>`;
  let logsBlock = "";
  // Suppressed while Details are open: the expansion's
  // latest-lines block shows the same stored tail (all 5 lines)
  // directly above, and the lines render in one place only.
  if (b.state === "failed" && !detailsOpen) {
    const tail = logTails.get(b.id);
    // An empty tail renders no block at all (principle 13) — the
    // full log is one click away (View all logs beside the
    // caption). The caption's qualifier rides its
    // info-tip. The stored tail holds 5 lines (the Details
    // block's count); this diagnostic block keeps its last 3.
    const lines = tail && tail.length
      ? `<div style="margin-top:8px;font-size:11px;color:var(--text3);display:flex;align-items:baseline;gap:8px">Latest output<span class="info-tip" title="stderr preferred — that is where llama-server reports load failures">${icon("info")}</span><a href="#/logs?scope=${esc(encodeURIComponent(b.id))}&follow=1" style="margin-left:auto">View all logs</a></div><div class="logstream" style="margin-top:4px">${tail.slice(-3).map(logLineHtml).join("")}</div>`
      : "";
    logsBlock = lines;
  }
  // GPU residency for this backend, as bars (Pete's card redesign,
  // 2026-10-16): one horizontal bar per GPU, name at left, GiB at
  // right, filled to the backend's share of that GPU's total VRAM.
  // Measured per-process telemetry is authoritative while the
  // backend is active; under 1 GiB is a CUDA context only and
  // renders as a striped sliver (its tooltip says so). When the
  // backend is inactive there is no measured residency, so the bars
  // are ghost fills of the configured tensor split — where the
  // model will load at its next start. An unmeasured GPU (mib null
  // — WDDM, counter data unavailable) gets an empty bar; "not
  // measured" exists only as its tooltip, never as inline text.
  const res = residency.get(b.id) ?? [];
  const gpuColors = ["#4a9eff", "#a371f7", "#39c5cf", "#d29922", "#3fb950", "#f85149"];
  const splitEntries = (b.tensor_split ?? [])
    .map((frac, index) => ({ index, frac }))
    .filter((e) => e.frac > 0);
  const gpuLabel = (index: number): string => {
    const measured = res.find((r) => r.gpu === index);
    return `GPU ${index}${(measured?.name ?? gpuNames.get(index)) ? ` · ${esc(measured?.name ?? gpuNames.get(index) ?? "")}` : ""}`;
  };
  const splitSummary = splitEntries.length
    ? splitEntries.map((e) => `${gpuLabel(e.index)} — ${Math.round(e.frac * 100)}%`).join(" · ")
    : "";
  const barRow = (index: number, fillPct: number, valText: string, cls = "", tip = ""): string => `<div class="res-bar-row"${tip ? ` title="${esc(tip)}"` : ""}>
      <span class="res-bar-name">${gpuLabel(index)}</span>
      <span class="res-bar-track${cls === "ghost" ? " ghost" : ""}"><i class="${cls}" style="width:${Math.min(100, Math.max(0, fillPct)).toFixed(1)}%;background-color:${gpuColors[index % gpuColors.length]}"></i></span>
      <span class="res-bar-val">${valText}</span></div>`;
  let gpuHtml = "";
  if (b.configured) {
    const knownRes = res.filter((r) => r.mib !== null);
    let bars = "";
    let basis = "";
    let titleTip = "";
    // Configured GPUs no process figure came for are listed too, as not
    // measured — a two-GPU backend never reads as a one-GPU one.
    const resAll: ResidencyRow[] = [
      ...res,
      ...(active && res.length ? configuredGpus(b).filter((i) => !res.some((r) => r.gpu === i)).map((i) => ({ gpu: i, name: gpuNames.get(i) ?? "", mib: null })) : []),
    ].sort((x, z) => x.gpu - z.gpu);
    if (knownRes.length) {
      basis = "measured";
      bars = resAll.map((r) => {
        if (r.mib === null) return barRow(r.gpu, 0, "", "", "Not measured — per-process usage is not measured on this GPU; placement follows the configured split.");
        const total = gpuTotalsMiB.get(r.gpu) ?? 0;
        const share = total > 0 ? (r.mib / total) * 100 : 0;
        const gib = `${fmtGb(r.mib / 1024)} GiB`;
        return r.mib < 1024
          ? barRow(r.gpu, share, gib, "ctx", `Context only: ${gib} — under 1 GiB on a GPU is the CUDA context only, not model weights.`)
          : barRow(r.gpu, share, gib);
      }).join("");
    } else if (res.length) {
      basis = "not measured";
      titleTip = "Per-process usage is not measured on this GPU — placement follows the configured split.";
      bars = resAll.map((r) => barRow(r.gpu, 0, "", "", "Not measured — per-process usage is not measured on this GPU; placement follows the configured split.")).join("");
    } else if (!active) {
      // Ghost bars from the configured split; an unsplit backend
      // loads whole on GPU 0 (the app's standing placement default,
      // the same basis the dashboard placements use).
      const ghost = splitEntries.length ? splitEntries : [{ index: 0, frac: 1 }];
      basis = "configured";
      bars = ghost.map((e) => barRow(e.index, e.frac * 100, `${Math.round(e.frac * 100)}%`, "ghost", "Configured split — this share of the model loads on this GPU at the backend's next start.")).join("");
    }
    // An active backend with no attributed process yet has nothing
    // real to encode; the section appears with the first sample.
    if (bars) {
      gpuHtml = `<div class="backend-gpus">
      <div class="backend-gpus-title">GPUs (${basis})${titleTip ? `<span class="info-tip" title="${esc(titleTip)}">${icon("info")}</span>` : ""}</div>
      ${bars}
    </div>`;
    }
  }
  // Gateway statistics for this backend (from the LiteLLM
  // metrics store). The card carries Throughput only: Requests
  // (1h) and Errors (1h) already live in the view's overview
  // list (numbers in one place, principle 6).
  // The stat tiles: live Throughput (the recent window's rate:
  // the backend's own /slots rate when available, the gateway
  // live rate as fallback — present only when the poller
  // actually measured a current rate, absent, never 0, when
  // neither signal is available) and Predicted throughput
  // (the saved configuration's exact-match measurement, design
  // §7 — generation only, the rest in the tooltip).
  const statTiles: [string, string, string | null][] = [];
  if (b.throughput_toks !== undefined) {
    statTiles.push(["Throughput", `${trim1(b.throughput_toks)} tok/s`, null]);
  }
  // Predicted throughput renders on EVERY card (Pete,
  // 2026-10-20): with no exact measurement the tile carries the
  // explicit "Not measured" empty state — never 0, never an
  // omitted field (principle 12) — and the Refresh control
  // beside it runs the standardized benchmark (the Optimize
  // Measure profile, the one whose entries feed the prediction)
  // to fill or update it. Measuring requires a running backend
  // (the measure op's own rule), so a stopped or unconfigured
  // backend's control is disabled with the reason as its title.
  const predictedTile = ((): string => {
    const startedAt = predictRefreshing.get(b.id);
    const fig = b.predicted && b.predicted.predicted_per_second !== null
      ? b.predicted.predicted_per_second
      : null;
    const canMeasure = b.configured && b.state === "running";
    const refreshTitle = startedAt !== undefined
      ? "Measurement running"
      : !b.configured
        ? "Not in the configuration — nothing to measure"
        : b.state !== "running"
          ? "Backend is not running — start it to refresh the prediction"
          : "Refresh the prediction — runs the standardized benchmark";
    const refreshBtn = `<button class="btn" data-back-act="predict-refresh" data-back-id="${esc(b.id)}" style="padding:2px 8px;font-size:11px" title="${esc(refreshTitle)}" aria-label="Refresh predicted throughput for ${esc(b.id)}"${startedAt !== undefined || !canMeasure ? " disabled" : ""}>${icon("restart")} Refresh</button>`;
    const tip = fig !== null && b.predicted
      ? predictedCardTip(b.predicted)
      : "No measurement of this backend's saved configuration yet. Refresh runs the standardized benchmark (the Optimize page's Measure action) against the running backend and stores the result as the prediction.";
    const value = startedAt !== undefined
      ? `Measuring <span data-pred-elapsed="${esc(b.id)}">${fmtElapsed(Math.max(0, Math.floor((Date.now() - startedAt) / 1000)))}</span>`
      : fig !== null
        ? `${trim1(fig)} tok/s`
        : `<span style="color:var(--text3);font-weight:400">Not measured</span>`;
    return `<div class="kv backend-stats-predicted"><span class="k">Predicted throughput<span class="info-tip" title="${esc(tip)}">${icon("info")}</span></span><span class="v" style="font-size:13px">${value} ${refreshBtn}</span></div>`;
  })();
  const statsHtml = `<div class="backend-stats">
        ${statTiles
          .map(([k, v, tip]) => `<div class="kv"><span class="k">${esc(k)}${tip ? `<span class="info-tip" title="${esc(tip)}">${icon("info")}</span>` : ""}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
        ${predictedTile}
      </div>`;
  // Offload guardrail and probe lines for the saved position
  // (at rest, not the flags-form draft). Status colors follow the
  // design principles: light red for blocked/failed, amber for
  // transitional/warning, gray for the neutral custom spelling.
  // Nothing renders when none of the states apply (principle 13).
  // A blocked-and-unresolved position carries the one-click
  // resolving action; a custom spelling in that same state reuses
  // that button rather than duplicating it.
  const offloadStatusHtml = (() => {
    const o = b.offload;
    const probe = b.offload_probe;
    if (!o && !probe) return "";
    const lines: string[] = [];
    const line = (color: string, inner: string): string =>
      `<div style="font-size:12px;color:${color};margin-top:6px">${inner}</div>`;
    const g = o?.guardrail ?? null;
    if (o && g?.status === "blocked" && !g.overridden) {
      // The blocked fact is a launch-preflight fact and renders in
      // both modes; the one-click resolving action is a flags
      // write, so in advanced flags mode (tracker #60) the line
      // carries no button — the editor is the remedy, and the
      // server refuses the structured write anyway. The deliberate
      // override is different: it writes only the activation
      // journal, never the flags, so its action renders in both
      // modes — a hand-edited blocked position is exactly the case
      // it exists for. The override is operation-critical, so it
      // keeps words + icon and takes a confirmation step whose
      // button names the exact position (layers + placed CC set)
      // being overridden (the armed state, the remove button's
      // idiom).
      const ccs = (g.placed_ccs ?? []).join(", ") || g.compute_capability;
      const overrideBtn = confirmOverride.has(b.id)
        ? `<button class="btn danger armed" data-back-act="offload-override-confirm" data-back-id="${esc(b.id)}"${dis}>${icon("warn")} Confirm override: ${o.layers} of ${o.moe_layers} layers, CC ${esc(ccs)}</button>`
        : `<button class="btn" data-back-act="offload-override-arm" data-back-id="${esc(b.id)}"${dis}>${icon("warn")} Override start block</button>`;
      lines.push(line("#ff7b72", b.flags_advanced
        ? `Start blocked: offload ${o.layers} of ${o.moe_layers} layers is outside the verified range for this GPU ${overrideBtn}`
        : `Start blocked: offload ${o.layers} of ${o.moe_layers} layers is outside the verified range for this GPU <button class="btn" data-back-act="offload-set-verified" data-back-id="${esc(b.id)}"${dis}>${icon("check")} Set to ${g.nearest_verified_layers} of ${o.moe_layers} layers</button> ${overrideBtn}`));
    } else if (o && g?.status === "blocked" && g.overridden) {
      // The overridden state is distinct (amber, named as an
      // override) and carries the clear action: clearing drops the
      // journal record and the position is blocked again.
      const ccs = (g.placed_ccs ?? []).join(", ") || g.compute_capability;
      lines.push(line("#e3b341", `Offload override recorded: ${o.layers} of ${o.moe_layers} layers is unverified on this GPU (CC ${esc(ccs)}) <button class="btn" data-back-act="offload-override-clear" data-back-id="${esc(b.id)}"${dis}>${icon("ban")} Clear override</button>`));
    } else if (o && g?.status === "untested") {
      lines.push(line("#e3b341", `Partial offload: unverified on this GPU`));
    }
    // The "custom spelling" line is a structured-form readout:
    // in advanced mode the operator's own string is displayed
    // verbatim (the raw flags line), so the line is redundant.
    if (o?.custom && !b.flags_advanced) {
      lines.push(line("var(--text2)", `Offload: Custom (${o.layers} of ${o.moe_layers} layers)`));
    }
    if (probe?.state === "probing") {
      lines.push(line("#e3b341", o ? `Probing offload: ${probe.layers} of ${o.moe_layers} layers` : `Probing offload: ${probe.layers} layers`));
    } else if (probe?.state === "failed") {
      const detail = (probe.detail ?? "").trim();
      const inner = !detail
        ? "Offload probe failed"
        : detail.length <= 120
          ? `Offload probe failed \u2014 ${esc(detail)}`
          : `Offload probe failed<span class="info-tip" title="${esc(detail)}">${icon("info")}</span>`;
      lines.push(line("#ff7b72", inner));
    }
    // A passed probe is transient assurance, not card state: no line.
    return lines.join("");
  })();
  // At-rest MTP lines (tracker #53), in the offload status
  // block's idiom: a non-standard saved spelling, and the
  // --parallel conflict when MTP is on — surfaced, never fixed
  // silently. Nothing renders in the ordinary states.
  const mtpStatusHtml = (() => {
    const m = b.mtp;
    // Advanced flags mode (tracker #60): both lines decode the
    // raw flags into structured terms the mode supersedes. The
    // raw string itself is displayed verbatim on the card, so a
    // custom spelling or a --parallel conflict is visible in it
    // directly, and the flags editor is the remedy the conflict
    // copy points at.
    if (!m || b.flags_advanced) return "";
    const lines: string[] = [];
    if (m.custom) {
      lines.push(`<div style="font-size:12px;color:var(--text2);margin-top:6px">MTP: Custom (${esc(mtpSavedTokenText(m))})</div>`);
    }
    if (m.enabled && m.parallel !== 1) {
      lines.push(`<div style="font-size:12px;color:#e3b341;margin-top:6px">${esc(mtpConflictText(m.parallel))}</div>`);
    }
    return lines.join("");
  })();
  // The flags editor renders inside this card (Pete's request,
  // 2026-10-16): preset dropdowns built from the model's rated
  // context, with a Custom numeric input when the value is not a
  // preset. Field events sync to the wire-level draft state, so the
  // poll repaint re-renders the form without losing input.
  // The expert-offload slider rides the same panel when the
  // backend's model is offload-capable: its position is the number
  // of MoE layers whose routed experts sit in system RAM. The
  // track's green band ends at the model's green limit (its
  // catalog default offload — the most offload that stays fast);
  // yellow and red split the range past it. Sliding updates the
  // readout in place; like every control in the panel the position is
  // a draft until Save.
  const offloadHtml = flags && b.offload && !flags.modelChanged ? (() => {
    const o = b.offload;
    const n = flags.offloadLayers ?? o.layers;
    // A released position the guardrail blocks needs a decision before
    // Save: save it with an override, or snap to the nearest verified
    // position. Both actions are operation-critical, so both keep
    // their words and an icon.
    const pending = flags.offloadPending;
    const decisionHtml = pending && o.guardrail && offloadIsUntested(o, pending.layers) ? `<div data-offload-decision style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:8px">
      <span data-offload-decision-text style="font-size:12px;color:#e3b341">${pending.layers} of ${o.moe_layers} layers is unverified on this GPU${pending.override ? " \u2014 Save records an override" : ""}</span>
      ${pending.override ? "" : `<button class="btn" data-back-act="offload-override" data-back-id="${esc(b.id)}"${dis}>${icon("warn")} Save with override</button>`}
      <button class="btn" data-back-act="offload-snap" data-back-id="${esc(b.id)}"${dis}>${icon("check")} Snap to ${o.guardrail.nearest_verified_layers} of ${o.moe_layers} layers</button>
    </div>` : "";
    return offloadSliderHtml({ offload: o, layers: n, ownerId: b.id, disabled: inFlight.has(b.id), decisionHtml });
  })() : "";
  // The expert-cache control, directly under the offload slider
  // in the same panel: the shared enable switch (rule 16) plus a
  // size select in the ctx/batch selects' idiom — the presets,
  // Auto, and the saved value as its own option when it is not a
  // preset — whose state lives in the wire-level draft
  // (flags.moeCacheOn / flags.moeCacheMib), with the token
  // readout naming the flag the value writes. Rendered only for
  // offload-capable backends (the payload carries `moe_cache` —
  // the offload slider's eligibility). Both controls are
  // disabled while the drafted offload is 0: with no experts in
  // system RAM there is nothing for the cache to hold, and the
  // label's tip says so.
  const moeCacheHtml = flags && b.moe_cache && !flags.modelChanged ? (() => {
    const mc = b.moe_cache;
    const on = flags.moeCacheOn;
    const size = flags.moeCacheMib ?? mc.mib ?? MOE_CACHE_DEFAULT_MIB;
    const noOffload = (flags.offloadLayers ?? mc.offload_layers) === 0;
    const ctlDisabled = inFlight.has(b.id) || noOffload;
    const tip = noOffload
      ? "Disabled: this backend offloads 0 MoE layers, so no experts sit in system RAM for the cache to hold. Raise the offload above 0 to enable it."
      : "Keeps the most recently used experts of the offloaded (system-RAM) layers in GPU memory and computes them there; experts not in the cache still run on the CPU. The size is priced into the backend's memory requirement, and a start that cannot fit it is refused with the numbers. The best size is empirical — the optimizer can sweep it once that support lands. Applies on the backend's next start.";
    const sizeOptions = MOE_CACHE_PRESETS_MIB.includes(size) ? MOE_CACHE_PRESETS_MIB : [...MOE_CACHE_PRESETS_MIB, size].sort((x, y) => x - y);
    const draftIsSaved = !flags.moeCacheAuto && on === (mc.mib != null) && (!on || size === mc.mib);
    return `<div style="margin-top:12px">
      <div style="font-size:12px;color:var(--text2)">Expert cache<span class="info-tip" title="${esc(tip)}">${icon("info")}</span></div>
      <div style="display:flex;gap:6px;margin-top:4px;align-items:center">
        ${enableSwitch({ on, name: `Expert cache for ${b.id}`, attrs: `data-moe-cache-switch data-moe-cache-backend="${esc(b.id)}"`, disabled: ctlDisabled })}
        <select data-moe-cache-select data-moe-cache-backend="${esc(b.id)}" style="${SELECT_STYLE};flex:1;min-width:0" aria-label="Expert cache size for ${esc(b.id)}"${ctlDisabled ? " disabled" : ""}>
          ${sizeOptions.map((v) => `<option value="${v}"${!flags.moeCacheAuto && v === size ? " selected" : ""}>${v} MiB</option>`).join("")}
          <option value="auto"${flags.moeCacheAuto ? " selected" : ""}>Auto</option>
        </select>
      </div>
      <div style="font-size:12px;color:var(--text2);margin-top:4px"><span data-moe-cache-token style="font-family:var(--mono)">${esc(draftIsSaved ? moeCacheSavedTokenText(mc) : !on ? moeCacheTokenText(null) : flags.moeCacheAuto ? "--moe-cache-mib (size chosen at Save from free VRAM)" : moeCacheTokenText(size))}</span>${mc.custom && draftIsSaved ? `<span title="The saved flags spell the expert cache in a form the control does not write (an = spelling, a 0 token, ...). Saving from this control writes the canonical token; every other flag is preserved."> · custom flags</span>` : ""}</div>
    </div>`;
  })() : "";
  // The MTP draft control (tracker #53), directly under the
  // offload slider in the same panel: a dropdown over
  // Off/1/2/3/4 in the ctx/batch selects' idiom (SELECT_STYLE,
  // the current value as the selected option, always visible at
  // rest), whose value lives in the wire-level draft state
  // (flags.mtpPosition), with the token readout naming the
  // flags the value writes, and the --parallel conflict line
  // when the drafted slot count is anything other than 1.
  // Rendered only for MTP-capable backends (the payload carries
  // `mtp`). The readout shows the SAVED spelling until the value
  // changes; a backend on without an explicit length reads as
  // llama.cpp's default rather than an implied saved value.
  const mtpHtml = flags && b.mtp && !flags.modelChanged ? (() => {
    const m = b.mtp;
    const pos = flags.mtpPosition ?? mtpPosition(m);
    const conflict = pos > 0 && flags.fields.parallel !== 1;
    const mtpSaved = pos === mtpPosition(m);
    return `<div style="margin-top:12px">
      <label style="font-size:12px;color:var(--text2);display:block">MTP draft tokens<span class="info-tip" title="Speculative decoding: the model drafts up to this many tokens ahead and verifies them in a single pass. More draft tokens can generate faster, but drafts the verifier rejects are wasted work, so gains depend on how often drafts are accepted. Applies on the backend's next start.">${icon("info")}</span>
        <span style="display:flex;gap:6px;margin-top:4px">
          <select data-mtp-select data-mtp-backend="${esc(b.id)}" style="${SELECT_STYLE};flex:1;min-width:0" aria-label="MTP draft tokens for ${esc(b.id)}"${dis}>
            ${MTP_POSITIONS.map((v) => `<option value="${v}"${v === pos ? " selected" : ""}>${v === 0 ? "Off" : String(v)}</option>`).join("")}
          </select>
        </span></label>
      <div style="font-size:12px;color:var(--text2);margin-top:4px"><span data-mtp-token style="font-family:var(--mono)">${esc(mtpSaved ? mtpSavedTokenText(m) : mtpTokenText(pos))}</span>${m.custom && mtpSaved ? `<span title="The saved flags spell MTP in a form the control does not write (an = spelling, a shared --spec-type list, ...). Saving from this control writes the canonical tokens; every other flag is preserved."> · custom flags</span>` : ""}</div>
      <div data-mtp-conflict style="font-size:12px;color:#e3b341;margin-top:4px;${conflict ? "" : "display:none"}">${esc(mtpConflictText(flags.fields.parallel))}</div>
    </div>`;
  })() : "";
  // The Parallel slots field (tracker #61), under the ctx/batch
  // pair: a preset+custom select over the backend's --parallel
  // count, saved with the rest of the panel. The custom
  // marker shows while the draft still equals the saved value:
  // it describes the saved spelling, and a changed selection has
  // no saved spelling yet.
  const parallelHtml = flags && b.parallel ? flagsParallelHtml({
    value: flags.fields.parallel,
    custom: flags.fields.parallelCustom,
    savedCustom: b.parallel.custom && flags.fields.parallel === b.parallel.slots,
    fieldAttr: "data-flags-field",
    disabled: inFlight.has(b.id),
  }) : "";
  // The KV cache / flash-attention controls, under the parallel
  // field: selects saved with the rest of the panel, with the predicted footprint
  // and predicted-best action rendered from the preview payload
  // the wire layer prices for the current draft.
  const cacheSel: CacheSelectionLike = flags?.cacheSel ?? (b.cache ? { k: b.cache.k, v: b.cache.v, flash_attn: b.cache.flash_attn } : { k: "f16", v: "f16", flash_attn: "auto" });
  const cacheHtml = flags && b.cache ? flagsCacheHtml({
    sel: cacheSel,
    savedCustom: b.cache.custom && sameCacheSelection(cacheSel, b.cache),
    savedAbsent: !b.cache.present && sameCacheSelection(cacheSel, b.cache),
    preview: flags.cachePreview,
    fieldAttr: "data-cache-field",
    bestAttr: `data-cache-best data-cache-backend="${esc(b.id)}"`,
    disabled: inFlight.has(b.id),
  }) : "";
  // Advanced flags mode (tracker #60): the panel is REPLACED by
  // the raw-flags text editor — the structured controls are not
  // rendered at all, so nothing inert invites an edit the server
  // would refuse. The textarea pre-fills with the backend's saved
  // raw flags (the string the structured decoders read); text
  // edits sync to the wire-level draft, so poll repaints keep the
  // text and the warning list renders live under the box.
  // The Edit panel's Model field (Pete's report, 2026-10-20):
  // the model is chosen here, as a draft, and applied through
  // model_assign only when the panel saves — the card face no
  // longer carries a model control that assigns on selection.
  // The select renders the DRAFT as selected (modelOptions'
  // selectedId), so a poll repaint shows the operator's unsaved
  // choice. Rendered in both panel variants: model_assign
  // preserves an advanced backend's raw flags verbatim
  // (flags_untouched), so the field is safe in advanced mode too.
  const editModelHtml = flags ? `<div style="margin-top:10px">
      <label style="font-size:12px;color:var(--text2);display:block">Model<span class="info-tip" title="The model this backend serves. The change applies only when you save from this panel; a running backend picks the new model up on its next start. Offload, expert cache and MTP depend on the model and are edited after its change is saved.">${icon("info")}</span>
        <span style="display:flex;gap:6px;margin-top:4px">
          <select data-edit-model="${esc(b.id)}" style="${SELECT_STYLE};flex:1;min-width:0" aria-label="Model for ${esc(b.id)}"${dis}>${modelOptions(b, models, flags.editModelId)}</select>
        </span></label>
    </div>` : "";
  // The Placement section of the Edit panel (moved from the GPUs page,
  // Pete's direction, 2026-10-16): the tensor split is a per-backend
  // setting, edited here like the flags and saved by the panel's Save.
  // Propose prices a selection without saving it. State lives in the
  // wire layer, so poll repaints keep it intact.
  const placeDis = dis || (place && (place.busy || !place.modelId)) ? " disabled" : "";
  const projHtml = place && place.projection ? (() => {
    const p = place.projection;
    // When the running backend's current allocation is credited,
    // the table shows the credit basis: what it already holds on
    // each GPU and the additional allocation that must fit in free
    // VRAM. Without a credit the two extra columns carry no
    // information and are omitted.
    const credited = (p.current_mib ?? []).some((v) => v > 0);
    const rows = p.split.map((frac, i) => {
      const gi = p.gpu_indices && p.gpu_indices[i] !== undefined ? p.gpu_indices[i] : i;
      const gname = place.gpus.find((g) => g.index === gi)?.name ?? `GPU ${gi}`;
      const free = p.free_mib && p.free_mib[i] !== undefined ? `${fmtCount(p.free_mib[i])} MiB` : "\u2014";
      const freeOk = p.fits_free?.[i] ?? (p.free_mib?.[i] !== undefined ? (p.additional_mib?.[i] ?? p.per_gpu_mib[i]) <= p.free_mib[i] : null);
      // With a backend in scope the row's state is the three-state
      // classification of the full requirement against total VRAM;
      // the weights-only projection keeps its free-VRAM wording.
      const st = p.fit_states?.[i];
      const fit = frac === 0 ? "not used"
        : st === "wddm_paged" ? `will page ${fmtCount(p.paged_mib?.[i] ?? 0)} MiB`
        : st === "does_not_fit" ? "does not fit"
        : st === "fits" ? (freeOk === false ? "not enough free now" : freeOk === true ? "fits" : "within total VRAM")
        : p.per_gpu_fits[i] ? "fits" : "does not fit";
      const creditCells = credited ? `<td>${fmtCount(p.current_mib?.[i] ?? 0)} MiB</td><td>${fmtCount(p.additional_mib?.[i] ?? p.per_gpu_mib[i])} MiB</td>` : "";
      return `<tr><td>${esc(gname)}</td><td>${Math.round(frac * 100)}%</td><td>${fmtCount(p.per_gpu_mib[i])} MiB</td>${creditCells}<td>${free}</td><td>${fit}</td></tr>`;
    }).join("");
    const ageMs = p.snapshot_age_ms ?? null;
    // The headline chip is the worst three-state classification
    // when the projection carries one (a backend in scope).
    const worstState = (p.fit_states ?? []).includes("does_not_fit") ? "does_not_fit"
      : (p.fit_states ?? []).includes("wddm_paged") ? "wddm_paged"
      : p.fit_states?.length ? "fits" : null;
    const pagedTotal = (p.paged_mib ?? []).reduce((a, v) => a + v, 0);
    const additional = p.split.reduce((a, _, i) => a + (p.additional_mib?.[i] ?? p.per_gpu_mib[i] ?? 0), 0);
    const freeAll = p.split.reduce((a, frac, i) => a + (frac > 0 ? p.free_mib?.[i] ?? 0 : 0), 0);
    const freeFits = p.split.map((frac, i) => frac === 0 || p.free_mib?.[i] === undefined ? null : (p.fits_free?.[i] ?? (p.additional_mib?.[i] ?? p.per_gpu_mib[i]) <= p.free_mib[i]));
    const nowFits = p.fits_now ?? (freeFits.some((x) => x === false) ? false : freeFits.some((x) => x === true) ? true : null);
    const shortAt = freeFits.findIndex((x) => x === false);
    const fitChip = fitChipHtml({
      state: worstState ?? (p.fits ? "fits" : "does_not_fit"), pagedMib: pagedTotal,
      fitsNow: nowFits,
      neededMib: shortAt >= 0 ? p.additional_mib?.[shortAt] ?? p.per_gpu_mib[shortAt] : additional,
      freeMib: shortAt >= 0 ? p.free_mib?.[shortAt] ?? null : freeAll,
      gpu: shortAt >= 0 ? p.gpu_indices?.[shortAt] ?? shortAt : null,
    });
    return `<div style="margin-top:10px">
      ${p.down_indices && p.down_indices.length ? `<div style="font-size:12px;color:var(--err);margin-bottom:6px" title="${p.down_indices.map((i) => `GPU ${i}`).join(", ")} ${p.down_indices.length === 1 ? "is" : "are"} down and ${p.down_indices.length === 1 ? "is" : "are"} treated as having no free VRAM.">${p.down_indices.map((i) => `GPU ${i}`).join(", ")}: down — no free VRAM</div>` : ""}
      <div style="font-size:12px">Fit: ${fitChip}<span class="info-tip" title="${p.weights_only === false ? "Projection prices the backend's full requirement: weights, KV cache, state cache, and the expert cache on the GPUs holding offloaded layers." : "Projection uses model weights only; run-time allocations add overhead."}">${icon("info")}</span></div>
      <table class="dtab" style="margin-top:8px"><thead><tr><th>GPU</th><th>Fraction</th><th>Required</th>${credited ? `<th title="The allocation this backend already holds on the GPU. Applying the split restarts the backend, freeing it first.">Current</th><th title="Required minus current: what must fit in free VRAM.">Additional</th>` : ""}<th>Free (current)</th><th>Fit</th></tr></thead><tbody>${rows}</tbody></table>
      ${p.moe_cache_mib ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Expert cache: ${fmtGb(p.moe_cache_mib / 1024)} GiB<span class="info-tip" title="The --moe-cache-mib budget, already inside the per-GPU Required figures: it lands only on the GPUs holding offloaded layers.">${icon("info")}</span></div>` : ""}
      ${p.sys_mib ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">System RAM: ${fmtGb(p.sys_mib / 1024)} GiB<span class="info-tip" title="Routed experts under expert offload. The per-GPU figures cover the GPU-resident requirement.">${icon("info")}</span></div>` : ""}
      <div style="font-size:12px;color:var(--text2);margin-top:6px">${ageMs === null ? "Snapshot: latest" : `Snapshot age: ${Math.round(ageMs / 1000)} s`}</div></div>`;
  })() : "";
  const placeHtml = place ? `<div class="backend-form-panel placement-panel">
      <div style="font-size:12px;font-weight:600">${icon("layers")} Placement</div>
      ${place.modelId
        ? ""
        : `<div style="font-size:12px;color:var(--err);margin-top:4px">Placement: unavailable<span class="info-tip" title="This backend's model file matches no configured model, so a split cannot be proposed or applied for it.">${icon("info")}</span></div>`}
      ${place.gpus.length ? `<div style="margin-top:8px">
        <span style="font-size:12px;color:var(--text2)">GPUs in the split:</span><span class="info-tip" title="The split divides the model's weights across the selected GPUs, in nvidia-smi index order. Uncheck a GPU to leave it out \u2014 for example the display GPU, whose memory Windows already uses. A changed selection is saved with Save, as an even division, and applies on the next start.">${icon("info")}</span>
        <div class="gpu-chip-row">${place.gpus.map((g) => {
          const on = !place.excluded.has(g.index);
          return `<label class="gpu-chip${on ? " on" : ""}${placeDis ? " is-disabled" : ""}" title="${esc(g.name)}"><span class="gpu-chip-top"><input type="checkbox" data-place-gpu="${g.index}" data-place-backend="${esc(b.id)}"${on ? " checked" : ""}${placeDis}> GPU ${g.index}</span><span class="gpu-chip-name">${esc(gpuShortName(g.name))}</span></label>`;
        }).join("")}</div>
      </div>`
      : `<div style="font-size:12px;color:var(--text2);margin-top:6px">GPUs: none reported<span class="info-tip" title="No GPU telemetry yet \u2014 a split cannot be proposed until telemetry reports the GPUs.">${icon("info")}</span></div>`}
      ${projHtml}
      <div class="card-actions"><button class="btn" data-back-act="place-propose" data-back-id="${esc(b.id)}" title="Propose split"${placeDis || !place.modelId || !place.gpus.length ? " disabled" : ""}>${icon("search")} Propose split</button></div>
    </div>` : "";
  const flagsHtml = flags ? (flags.advanced ? `<div class="backend-form-panel flags-panel">
      <div style="font-size:12px;font-weight:600">${icon("pencil")} Edit ${esc(b.id)}</div>
      ${editModelHtml}
      <div style="font-size:12px;color:var(--text2);margin-top:6px">Mode: Advanced<span class="info-tip" title="In advanced mode the text below is the backend's launch flags, edited directly. Context, batch, offload, MTP and placement are all set by this string; the structured form and its controls are disabled for this backend until you switch back. Warnings never block a save — unknown flags pass through to llama-server, which decides.">${icon("info")}</span></div>
      <textarea data-flags-raw="${esc(b.id)}" rows="4" spellcheck="false" style="width:100%;margin-top:8px;background:var(--panel2);border:1px solid var(--border);border-radius:8px;color:var(--text);padding:8px 10px;font-family:var(--mono);font-size:12px" aria-label="Raw flags for ${esc(b.id)}">${esc(flags.rawText)}</textarea>
      <div data-flags-warnings>${flagsRawWarningsHtml(flags.rawWarnings)}</div>
      ${placeHtml}
      <div data-predicted-slot>${predictedHtml}</div>
      <div class="card-actions"><button class="btn" data-back-act="flags-raw-save" data-back-id="${esc(b.id)}"${dis}>${icon("check")} Save</button>
        <button class="btn" data-back-act="flags-mode-structured" data-back-id="${esc(b.id)}"${dis}>${icon("gear")} Use structured form</button>
        <button class="btn icon-only" data-back-act="flags-cancel" data-back-id="${esc(b.id)}" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
    </div>` : `<div class="backend-form-panel flags-panel">
      <div style="font-size:12px;font-weight:600">${icon("pencil")} Edit ${esc(b.id)}</div>
      ${editModelHtml}
      ${flagsCtxBatchHtml({ ctx: flags.fields.ctx, batch: flags.fields.batch, ctxCustom: flags.fields.ctxCustom, batchCustom: flags.fields.batchCustom, ctxMax: flags.ctxMax, fieldAttr: "data-flags-field", disabled: inFlight.has(b.id) })}
      ${parallelHtml}
      ${cacheHtml}
      ${offloadHtml}
      ${moeCacheHtml}
      ${mtpHtml}
      ${flagsRatedLineHtml({ ctxMax: flags.ctxMax, modelId: flags.modelId, savedText: "Saved to the configuration with Save; applies on the backend's next start." })}
      ${placeHtml}
      <div data-predicted-slot>${predictedHtml}</div>
      <div class="card-actions"><button class="btn" data-back-act="flags-save" data-back-id="${esc(b.id)}"${dis}>${icon("check")} Save</button>
        <button class="btn" data-back-act="flags-mode-advanced" data-back-id="${esc(b.id)}"${dis}>${icon("pencil")} Edit flags as text</button>
        <button class="btn icon-only" data-back-act="flags-cancel" data-back-id="${esc(b.id)}" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
    </div>`) : "";
  // Basis of the memory figures (exact | catalog | estimate): the
  // explanation lives in a tooltip on the RAM line; the word itself
  // stays inline only for estimate, as the qualifier it was before.
  const basisTip = b.memory_basis === "exact"
    ? "Basis: exact — measured from the downloaded model file."
    : b.memory_basis === "catalog"
      ? "Basis: catalog — from the catalog entry for this model (used before download)."
      : b.memory_basis === "estimate"
        ? "Basis: estimate — estimated figures, not measured from the model file."
        : "";
  // Metric strip (Pete's card redesign): one row of labeled
  // single-value tiles — Model, Context, GPU RAM, System RAM, KV
  // cache, State cache. No sentences; the model tile is a VALUE (the served
  // model's short name, the file path in its tooltip) — the old
  // inline select assigned on selection with no save, so model
  // changes moved into the Edit panel (Pete's report, 2026-10-20).
  const matchedModel = models.find((m) => sameModelFile(b.model_file, m.gguf_path));
  const modelTileVal = b.configured
    ? `<span class="backend-model-file" title="${esc(b.model_file)}">${esc(matchedModel ? modelShortName(matchedModel) : modelFileShortName(b.model_file))}</span>`
    : `<span class="backend-model-file" title="${esc(b.model_file)}">${esc(b.model_file)}</span>`;
  const stripTiles: [string, string][] = [["Model", modelTileVal]];
  // Advanced flags mode (tracker #60): -c is still passed from the
  // configuration unless the raw flags carry their own context token,
  // which llama-server then uses (the last one wins).
  const rawCtx = b.flags_advanced ? rawContext(b.raw_flags ?? "") : null;
  if (b.n_ctx !== undefined) stripTiles.push(["Context", rawCtx !== null ? `<span title="Set by the raw flags">${fmtCount(rawCtx)}</span>` : fmtCount(b.n_ctx)]);
  if (b.gpu_ram_mib !== undefined) {
    // GPU RAM is everything the backend puts on the GPUs; its parts
    // follow as their own tiles.
    const parts: [string, number | null | undefined][] = [["weights", b.gpu_ram_mib], ["KV cache", b.kv_ram_mib], ["state cache", b.rs_ram_mib], ["expert cache", b.moe_cache_mib]];
    const gpuTotal = parts.reduce((a, [, v]) => a + (v ?? 0), 0);
    const tip = parts.filter(([, v]) => v).map(([k, v]) => `${k} ${fmtGb((v ?? 0) / 1024)} GiB`).join(" + ");
    stripTiles.push(["GPU RAM", `<span title="${esc(tip)}">${fmtGb(gpuTotal / 1024)} GiB</span>`]);
    stripTiles.push(["Weights", `<span title="GPU-resident model weights">${fmtGb(b.gpu_ram_mib / 1024)} GiB</span>`]);
    // An absent sys_ram_mib is unknown, never 0 (principle 12): the
    // tile renders only when the payload carries the figure.
    if (b.sys_ram_mib !== undefined) stripTiles.push(["System RAM", `${fmtGb(b.sys_ram_mib / 1024)} GiB`]);
    if (b.kv_ram_mib != null) stripTiles.push(["KV cache", `<span title="KV cache at the configured context">${fmtGb(b.kv_ram_mib / 1024)} GiB</span>`]);
    if (b.rs_ram_mib != null) stripTiles.push(["State cache", `<span title="Recurrent-state cache at the configured slot count">${fmtGb(b.rs_ram_mib / 1024)} GiB</span>`]);
    if (b.moe_cache_mib) stripTiles.push(["Expert cache", `<span title="MoE expert cache (--moe-cache-mib) priced into the requirement">${fmtGb(b.moe_cache_mib / 1024)} GiB</span>`]);
  }
  // The three-state fit verdict for the saved configuration, from
  // the server's requirement model (the start gate's numbers):
  // one chip (fitChipHtml), judged against the memory free now for a
  // backend that is not loaded. Absent when the server could not judge
  // the backend.
  const fitHtml = b.fit ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Fit: ${fitChipHtml({
    state: b.fit.state, pagedMib: b.fit.paged_mib,
    // A loaded backend already holds its memory: total VRAM decides.
    ...(st === "running" ? { fitsNow: true } : fitNow(b, gpuFreeMiB, residency.get(b.id) ?? [])),
  })}</div>` : "";
  // The strip is a wrapping flex row (styles.css .backend-strip),
  // never a single-row grid sized to the tile count: bare fr
  // tracks cannot shrink below their tiles' min-content widths,
  // so the full tile set (Model through State cache) overflowed
  // the card and the last tile painted outside it (Pete's
  // report, 2026-10-22). Wrapping grows the card to fit; the
  // Model tile keeps its double share of a shared row.
  const stripHtml = `<div class="backend-strip">
      ${stripTiles.map(([k, v], i) => `<div class="kv${i === 0 ? " backend-strip-model" : ""}"><span class="k">${k}</span><span class="v" style="font-size:13px">${v}</span></div>`).join("")}
    </div>`;
  // Details expandable: everything demoted off the closed card —
  // State/PID/Uptime/Batch, the configured-split percentages, and
  // the memory basis (its explanation rides the info-tip). Open
  // state lives in the wire layer, so poll repaints keep it.
  // State is not repeated here — the header's state chip carries it
  // (numbers and states live in one place, principle 6). The
  // section renders BELOW the actions bar in the card template
  // (Pete's declutter, 2026-10-20).
  const detailKv: [string, string][] = [
    ["PID", b.pid !== null ? String(b.pid) : "—"],
    ["Uptime", fmtUptime(b.uptime_secs)],
  ];
  // Batch is a structured flags readout like Context: omitted in
  // advanced flags mode (tracker #60) rather than shown as fact.
  if (!b.flags_advanced && b.n_batch != null) detailKv.push(["Batch", fmtCount(b.n_batch)]);
  // Tool-call probe state (MCP design §7.2): the persisted probe
  // outcome, or "not measured" when the backend has never been
  // probed (principle 12 — absence is not a measured state). It
  // rendered on the card face until the 2026-10-20 declutter
  // moved it here; the probe itself runs from the menu's Test
  // tool calls item. Supervisor-only orphan rows carry no probe
  // and no tile.
  if (b.configured) detailKv.push(["Tool calls", b.tool_call_probe ? b.tool_call_probe.outcome : "not measured"]);
  // Offload Layers: the saved expert-offload position, from the
  // same offload info the Edit panel's slider renders. A model
  // with no routed experts has no offload position to report —
  // the value says so plainly, never 0 (principle 12).
  if (b.configured) {
    detailKv.push(["Offload Layers", b.offload
      ? b.offload.layers <= 0
        ? `Off (0 of ${b.offload.moe_layers})`
        : b.offload.layers >= b.offload.moe_layers
          ? `Full (${b.offload.moe_layers} of ${b.offload.moe_layers})`
          : `${b.offload.layers} of ${b.offload.moe_layers}`
      : "Not applicable"]);
  }
  // Latest log lines: the stored tail for this backend (the same
  // get_logs records behind View all logs, with the
  // failed block's stderr preference). The block renders only
  // once a fetch has landed — an unfetched tail omits it
  // (principle 13); a fetched-but-empty tail states the fact.
  // The caption carries its own View all logs link to the
  // Logs view filtered to this backend (same href as the
  // card menu's item), so the full set is one click away.
  const detailTail = logTails.get(b.id);
  const logLinesHtml = detailTail === undefined ? "" : `<div style="margin-top:10px">
      <div style="font-size:11px;color:var(--text3);display:flex;align-items:baseline;gap:8px">Latest log lines<span class="info-tip" title="The backend's most recent log lines, newest last — the same records the Logs view shows for this backend.">${icon("info")}</span><a href="#/logs?scope=${esc(encodeURIComponent(b.id))}&follow=1" style="margin-left:auto">View all logs</a></div>
      ${detailTail.length ? `<div class="logstream backend-logtail" style="margin-top:4px">${detailTail.map(logLineHtml).join("")}</div>` : `<div style="font-size:12px;color:var(--text2);margin-top:4px">None recorded</div>`}
    </div>`;
  const detailsHtml = detailsOpen ? `<div class="backend-details">
      <div class="backend-detail-grid">
        ${detailKv.map(([k, v]) => `<div class="kv"><span class="k">${k}</span><span class="v" style="font-size:13px">${esc(v)}</span></div>`).join("")}
      </div>
      ${splitEntries.length ? `<div style="font-size:12px;color:var(--text2);margin-top:6px;overflow-wrap:anywhere">Configured split: ${splitSummary}</div>` : ""}
      ${basisTip ? `<div style="font-size:12px;color:var(--text2);margin-top:6px">Basis: ${esc(b.memory_basis ?? "")}<span class="info-tip" title="${esc(basisTip)}">${icon("info")}</span></div>` : ""}
      ${logLinesHtml}
    </div>` : "";
  // Advanced flags mode (tracker #60): the saved raw flags ARE
  // the configuration the operator owns in this mode, so they
  // render verbatim in Label: Value form in place of the
  // structured readouts the mode supersedes. An empty string
  // renders no line (principle 13); the header chip carries the
  // mode itself.
  const rawFlagsHtml = b.flags_advanced && b.raw_flags
    ? `<div class="backend-raw-flags" style="font-size:12px;color:var(--text2);margin-top:8px">Flags: <span style="font-family:var(--mono);color:var(--text)">${esc(b.raw_flags)}</span></div>`
    : "";
  // The supervisor's recorded failure reason, in the failed
  // state only: it names the cause (e.g. the device-unavailable
  // fail-fast verdict) the state chip cannot. Principle 9 form —
  // a noun label and the value, no sentence.
  const failureHtml = (st === "failed" && b.failure_reason
    ? `<div style="font-size:12px;color:var(--err);margin-top:6px">Failure: ${esc(b.failure_reason)}</div>`
    : "")
    + (b.auto_restart && st !== "running"
      ? `<div style="font-size:12px;color:${b.auto_restart.gave_up ? "var(--err)" : "#e3b341"};margin-top:6px" data-auto-restart="${esc(b.id)}">${esc(autoRestartText(b.auto_restart))}</div>`
      : "");
  const detailsBtn = `<button class="btn icon-only" data-back-act="details" data-back-id="${esc(b.id)}" aria-expanded="${detailsOpen}" title="Details" aria-label="Details">${icon(detailsOpen ? "chevD" : "chevR")}</button>`;
  return `<div class="card backend-card" id="card-backend-${esc(b.id)}">
      <h3 class="backend-card-head">${lifecycleDot(st)} ${esc(b.id)} <span class="port">:${b.port}</span>
        <span class="spacer" style="flex:1"></span>
        ${b.configured ? "" : `<span class="chip status-tag status-off" title="${esc(orphanText)}">not in config</span>`}
        ${b.configured ? `<span class="chip status-tag ${b.enabled ? "status-on" : "status-off"}">${b.enabled ? "enabled" : "disabled"}</span>` : ""}
        ${b.adopted ? `<span class="chip status-tag status-on" title="${esc(adoptedText)}">adopted</span>` : ""}
        ${b.flags_advanced ? `<span class="chip status-tag" title="Advanced flags mode: this backend's launch flags are edited as raw text in the Flags panel, and the structured flags form is disabled for it.">advanced flags</span>` : ""}
        ${b.service_managed ? `<span class="chip status-tag status-off" title="Runs as a Windows service (SCM)">service</span>` : ""}
        ${lifecycleChip(st, { transition: b.transition ?? null, failure: b.failure_reason ?? null, detail: b.pid !== null ? `pid ${b.pid}` : undefined })}</h3>
      ${failureHtml}
      ${stripHtml}
      ${fitHtml}
      ${gpuHtml}
      ${statsHtml}
      ${rawFlagsHtml}
      ${offloadStatusHtml}
      ${mtpStatusHtml}
      ${flagsHtml}
      <div class="card-actions backend-actions">${detailsBtn}
        ${actions}
        ${advancedMenu}</div>
      <div class="back-msg" style="margin-top:8px;font-size:12px;min-height:16px">${msgHtml}</div>
      ${detailsHtml}
      ${logsBlock}
      ${argv}
    </div>`;
}

/** The Backends page's list controls, kept across visits. */
export const backendsView = pageState("backends", { sort: "name", state: "all", filter: "" });

export function renderBackends(): string {
  return `${topbar("Backends", `<button class="btn" id="backAdd" title="Add backend">${icon("plus")} Add backend</button> <button class="btn icon-only" id="backRefresh" title="Refresh" aria-label="Refresh">${icon("restart")}</button>`)}
  <div class="view"><div id="backBanner"></div>
  <div id="backOverview"></div>
  <div style="display:flex;gap:10px;align-items:center;margin-bottom:10px;flex-wrap:wrap">
    <label style="font-size:12px;color:var(--text2)">Sort
      <select id="backSortSel" style="${SELECT_STYLE}">
        ${([["name", "Name"], ["port", "Port"], ["state", "State"], ["requests", "Requests (1h)"]]).map(([v, l]) => `<option value="${v}"${backendsView.sort === v ? " selected" : ""}>${l}</option>`).join("")}
      </select></label>
    <label style="font-size:12px;color:var(--text2)">State
      <select id="backStateSel" style="${SELECT_STYLE}">
        <option value="all">All</option>
        ${LIFECYCLE_STATES.filter((s) => s !== "not_installed").map((s) => `<option value="${s}"${backendsView.state === s ? " selected" : ""}>${esc(META[s].label.charAt(0).toUpperCase() + META[s].label.slice(1))}</option>`).join("")}
      </select></label>
    <input id="backFilter" type="text" value="${esc(backendsView.filter)}" placeholder="Filter by name, group, or model" aria-label="Filter backends" style="min-width:220px">
  </div>
  <div id="backAddForm"></div>
  <div id="backBody">${loadingHtml("Loading backends\u2026")}</div></div>`;
}

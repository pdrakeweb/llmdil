/* Backends view: data loading, painting and every card action
   (split from static.ts). */

import { addBackend, applyBackendConfig, backendAddPreview, backendCachePreview, clearBackendOffloadOverride, deleteBackend, getActivationPlan, getBackendConfig, getBackendLaunchCommand, getBackends, getGatewayConfig, getGpuTelemetry, getLogs, getModels, measurementLookup, modelAssign, optimizeMeasureBackend, placementApply, placementPropose, setBackendCache, setBackendFlagsMode, setBackendMoeCache, setBackendMtp, setBackendOffload, setBackendOffloadOverride, setBackendParallel, setBackendRawFlags, testBackend, testBackendToolCall, updateBackendFlags } from "../api";
import type { ActivationPlanData, BackendAddPreview, BackendRow, CachePreview, CacheSelectionLike, CacheTypeName, FlashAttnName, GatewayGroup, LogRow, MeasurementLookupRequest, ModelRow, PlacementProjectionData } from "../api";
import { fmtCount, fmtGb, stackedBar, trim1 } from "../charts";
import { esc } from "../html";
import { icon } from "../icons";
import { META, fromWire, lifecycleLabel, runLifecycle, serviceStore } from "../lifecycle";
import type { ServiceSnapshot } from "../lifecycle";
import type { GpuInfo } from "../types";
import { activationPlanHtml, handlePlanAction, newPlanUi } from "./activationPlan";
import { backendCard } from "./backendCard";
import { CACHE_TYPES, FLASH_ATTN_MODES, MOE_CACHE_DEFAULT_MIB, PARALLEL_MAX, PARALLEL_MIN, applyFlagsField, cachePreviewHtml, cacheTokenText, cacheVQuantizedFaOff, flagsCacheHtml, flagsCtxBatchHtml, flagsParallelHtml, flagsRatedLineHtml, flagsRawWarningsHtml, moeCacheTokenText, mtpPosition, mtpTokenText, offloadFigures, offloadIsUntested, offloadSliderHtml, offloadTokenText, paintOffloadReadout, parallelTokenText, predictedLineHtml, rawFlagWarnings, sameCacheSelection } from "./backendFlags";
import type { FlagWarningLike, FlagsDraft, PlaceCardState } from "./backendFlags";
import { SELECT_STYLE, bannerHtml, emptyHtml, fmtBytes, fmtElapsed, gpuShortName, modelShortName, onOvGoto, ovStatusBtn, sameModelFile, setBanner } from "./common";
import type { CardNote } from "./common";

export async function wireBackends(root: HTMLElement): Promise<() => void> {
  const openArgv = new Set<string>();
  /** Backend whose advanced-options menu is open (at most one). */
  const openMenus = new Set<string>();
  /** Backends whose Details expandable is open. */
  const openDetails = new Set<string>();
  const argvCache = new Map<string, string[]>();
  const msgs = new Map<string, CardNote>();
  /** Recent log lines for failed backends, fetched lazily after paint. */
  const logTails = new Map<string, LogRow[]>();
  /** Ids whose log tail has been fetched (or is in flight) this mount. */
  const tailsWanted = new Set<string>();
  /** Ids with a Details-driven tail refetch in flight: the
   *  open-Details path refetches on each load cycle instead of
   *  once per mount, so it tracks its own in-flight set. */
  const tailsFetching = new Set<string>();
  /** Backend ids with an action invoke in flight; their card buttons
   *  render disabled (the set outlives any single repaint). */
  const inFlight = new Set<string>();
  /** Backends whose predicted-throughput refresh (a Measure
   *  run started from the card) is in flight, by start time.
   *  Held outside the repainted DOM like the other card state,
   *  so poll repaints keep the Measuring state; a refresh
   *  deliberately does NOT join inFlight — a measurement takes
   *  minutes and must not disable the card or stall the poll. */
  const predictRefreshing = new Map<string, number>();
  /** The 1 s elapsed ticker for in-flight prediction
   *  refreshes (the Optimize view's pattern). */
  let predictTimer: number | null = null;
  const stopPredictTicker = (): void => {
    if (predictTimer !== null) { window.clearInterval(predictTimer); predictTimer = null; }
  };
  const startPredictTicker = (): void => {
    if (predictTimer !== null) return;
    predictTimer = window.setInterval(() => {
      if (!predictRefreshing.size) { stopPredictTicker(); return; }
      root.querySelectorAll<HTMLElement>("[data-pred-elapsed]").forEach((el) => {
        const startedAt = predictRefreshing.get(el.getAttribute("data-pred-elapsed") ?? "");
        if (startedAt !== undefined) {
          el.textContent = fmtElapsed(Math.max(0, Math.floor((Date.now() - startedAt) / 1000)));
        }
      });
    }, 1000);
  };
  let lastRows: BackendRow[] = [];
  /** Measured GPU residency by backend id (telemetry attribution). */
  let residency = new Map<string, { gpu: number; name: string; mib: number | null }[]>();
  /** Backends list controls (sort / state filter / text filter),
   *  held outside the repainted DOM. */
  let backSort = "name";
  let backFilter = "";
  let backState = "all";
  /** Registered models for the per-card model dropdowns. */
  let lastModels: ModelRow[] = [];
  /** Gateway groups + port, for the add-backend form. */
  let lastGroups: GatewayGroup[] = [];
  let gatewayPort = 4000;
  /** Backend ids whose Remove button is armed for confirmation. */
  const confirmRemove = new Set<string>();
  /** Backends whose WDDM-paged Start is armed (first click done);
   *  the second click starts with the overcommit confirmation.
   *  Cleared on state change and whenever the fit stops being a
   *  paged fit — the delete-confirm lifecycle. */
  const armedWddmStart = new Set<string>();
  /** Backend ids whose guardrail Override button is armed for
   *  confirmation (the confirm button names the exact position). */
  const confirmOverride = new Set<string>();
  /** Gateway activation plan (saved changes awaiting a gateway
   *  restart), painted into #backPlan above the add form. */
  let lastPlan: ActivationPlanData | null = null;
  const planUi = newPlanUi();
  const paintPlan = (): void => {
    const host = root.querySelector("#backPlan");
    if (host) host.innerHTML = activationPlanHtml(lastPlan, planUi, "summary");
  };
  let disposed = false;
  /** Load sequence: only the most recently started load may paint. */
  let loadSeq = 0;
  /** The rows as the server last reported them (list_backends, or a
   *  fresher shared-store snapshot). `lastRows` is these rows with
   *  the effective lifecycle state applied (the store's pending
   *  overlay), so every card, chip, filter and button rule reads one
   *  state — a restart started on any surface shows "restarting…". */
  let wireRows: BackendRow[] = [];
  const effRow = (row: BackendRow): BackendRow => {
    const st = serviceStore.backendState(row);
    return { ...row, state: st, lifecycle: st, transition: serviceStore.transitionOf(`backend:${row.id}`, row.transition ?? null) };
  };
  const lcSignature = (rows: BackendRow[]): string =>
    rows.map((r) => `${r.id}:${r.state}:${r.transition?.kind ?? ""}:${r.transition?.origin ?? ""}`).join("|");

  const paint = (): void => {
    const b = root.querySelector("#backBody");
    if (!b) return;
    const sorted = (list: BackendRow[]): BackendRow[] => {
      const c = [...list];
      if (backSort === "port") c.sort((a, z) => a.port - z.port);
      else if (backSort === "requests") c.sort((a, z) => (z.requests_1h ?? -1) - (a.requests_1h ?? -1));
      else if (backSort === "state") c.sort((a, z) => a.state.localeCompare(z.state) || a.id.localeCompare(z.id));
      else c.sort((a, z) => a.id.localeCompare(z.id));
      return c;
    };
    // Overview list: one line per backend (all rows, ignoring the
    // list filter), with column headings, divider lines, and a
    // status button that scrolls to the backend's card. The GPUs
    // column names only GPUs where the backend holds at least 1 GiB
    // measured — under that it holds only a CUDA context there.
    const ov = root.querySelector("#backOverview");
    if (ov) {
      ov.innerHTML = lastRows.length ? `<div class="ovlist ov-b">
        <div class="ov-head"><span></span><span>Backend</span><span>State</span><span>Group</span><span>Port</span><span>GPUs (measured)</span><span>Requests (1h)</span><span>Errors (1h)</span></div>
        ${sorted(lastRows).map((row) => {
          const res = residency.get(row.id) ?? [];
          // Unmeasured rows (mib null) are neither residency nor
          // context: the column says "not measured" rather than
          // claiming either.
          const resident = res.filter((r) => r.mib !== null && r.mib >= 1024);
          const gpuTxt = resident.length ? resident.map((r) => `GPU ${r.gpu}`).join(", ")
            : res.some((r) => r.mib !== null) ? "context only"
            : res.length ? "not measured" : "\u2014";
          const rst = fromWire(row.lifecycle ?? row.state);
          const rlabel = lifecycleLabel(rst, row.transition ?? null);
          const rdot = META[rst].dot;
          const btn = ovStatusBtn(rdot, rdot === "ok" ? "check" : rdot === "err" ? "err" : META[rst].isTransition || rst === "degraded" ? "clock" : "pause", `card-backend-${row.id}`, `${row.id}: ${rlabel} — go to card`);
          return `<div class="ov-row">${btn}<span class="ov-name">${esc(row.id)}</span><span class="ov-dim">${esc(rlabel)}</span><span class="ov-dim">${esc(row.group ?? "ungrouped")}</span><span class="ov-dim">${row.port}</span><span class="ov-dim">${esc(gpuTxt)}</span><span class="ov-dim">${row.requests_1h !== undefined ? fmtCount(row.requests_1h) : "\u2014"}</span><span class="ov-dim">${row.errors_1h !== undefined ? fmtCount(row.errors_1h) : "\u2014"}</span></div>`;
        }).join("")}</div>` : "";
    }
    const f = backFilter.trim().toLowerCase();
    const viewRows = lastRows.filter((row) =>
      (backState === "all" || row.state === backState) &&
      (!f || row.id.toLowerCase().includes(f) || (row.group ?? "").toLowerCase().includes(f) || row.model_file.toLowerCase().includes(f)));
    // Cards are grouped by gateway group (Pete's request, 2026-10-15):
    // a section per group in the order groups first appear across
    // the rows (backend config order), ungrouped rows and orphans
    // last under "Ungrouped".
    const sections: { name: string | null; rows: BackendRow[] }[] = [];
    const byKey = new Map<string, { name: string | null; rows: BackendRow[] }>();
    for (const row of viewRows) {
      const key = row.group ?? "";
      let sec = byKey.get(key);
      if (!sec) {
        sec = { name: row.group ?? null, rows: [] };
        byKey.set(key, sec);
        sections.push(sec);
      }
      sec.rows.push(row);
    }
    const ordered = [
      ...sections.filter((s) => s.name !== null),
      ...sections.filter((s) => s.name === null),
    ];
    b.innerHTML = ordered.map((sec) => `
      <div class="group-box">
      <div class="sec-title" style="margin:0 0 10px">${sec.name !== null ? `Group: ${esc(sec.name)}` : "Ungrouped"} <span style="color:var(--text3);font-weight:400">— ${sec.rows.length} backend${sec.rows.length === 1 ? "" : "s"}</span></div>
      <div class="grid2">${sorted(sec.rows).map((row) => {
        const mdl = flagsFor === row.id ? lastModels.find((m) => sameModelFile(row.model_file, m.gguf_path)) : undefined;
        const flagsArg: FlagsDraft | null = flagsFor === row.id
          ? { ctx: flagsCtx, batch: flagsBatch, ctxMax: mdl?.ctx_max ? mdl.ctx_max : null, modelId: mdl?.id ?? null, offloadLayers: flagsOffloadLayers, offloadPendingLayers: offloadPending?.backendId === row.id ? offloadPending.layers : null, mtpPosition: flagsMtpPosition, moeCacheOn: flagsMoeCacheOn, moeCacheMib: flagsMoeCacheMib, parallel: flagsParallel, cacheSel: flagsCacheSel, cachePreview: flagsCachePreview, advanced: row.flags_advanced === true, rawText: flagsRawText, rawWarnings: flagsRawWarnings, editModelId: flagsEditModelId }
          : null;
        const pmdl = placeFor === row.id ? lastModels.find((m) => sameModelFile(row.model_file, m.gguf_path)) : undefined;
        const placeArg: PlaceCardState | null = placeFor === row.id
          ? {
            gpus: lastGpuList.map((g) => ({ index: Number(g.id), name: g.name })),
            excluded: placeExcluded,
            modelId: pmdl?.id ?? null,
            projection: placeProjection && placeProjection.backendId === row.id ? placeProjection.data : null,
            busy: placeBusy,
          }
          : null;
        return backendCard(row, openArgv, openMenus, argvCache, msgs, logTails, inFlight, lastModels, confirmRemove, residency, new Map(lastGpuList.map((g) => [Number(g.id), g.name])), flagsArg, flagsFor === row.id ? predictedHtml : "", placeArg, new Map(lastGpuList.map((g) => [Number(g.id), g.vram_total_gb * 1024])), openDetails.has(row.id), predictRefreshing, confirmOverride, armedWddmStart);
      }).join("")}</div>
      </div>`).join("");
    if (!viewRows.length && lastRows.length) b.innerHTML = emptyHtml("No backends match the current filter.");
  };

  const backSortSel = root.querySelector("#backSortSel") as HTMLSelectElement | null;
  const backStateSel = root.querySelector("#backStateSel") as HTMLSelectElement | null;
  const backFilterInput = root.querySelector("#backFilter") as HTMLInputElement | null;
  backSortSel?.addEventListener("change", () => { backSort = backSortSel.value; paint(); });
  backStateSel?.addEventListener("change", () => { backState = backStateSel.value; paint(); });
  backFilterInput?.addEventListener("input", () => { backFilter = backFilterInput.value; paint(); });

  /** Lazily fetch the last log lines for the cards that show
   *  them, and repaint. One stored tail (up to 5 lines) serves
   *  two blocks: a failed backend's diagnostic block — fetched
   *  once per mount, as it has always been — and the Details
   *  section's latest-lines block, fetched while Details are
   *  open and refreshed on each load cycle, repainting only when
   *  the tail actually changed. get_logs takes the bare process
   *  id (the same scope the Logs view sends); records arrive
   *  newest-first. Stderr lines are preferred — that is where
   *  llama-server reports load failures. */
  const fetchLogTails = async (): Promise<void> => {
    const rows = lastRows;
    let fetched = false;
    for (const row of rows) {
      const detailsWants = openDetails.has(row.id);
      if (row.state !== "failed" && !detailsWants) continue;
      if (detailsWants ? tailsFetching.has(row.id) : tailsWanted.has(row.id)) continue;
      if (detailsWants) tailsFetching.add(row.id); else tailsWanted.add(row.id);
      try {
        const r = await getLogs(row.id, { limit: 50 });
        if (disposed) return;
        if (r.error) { if (!detailsWants) tailsWanted.delete(row.id); continue; }
        const stderr = r.data.lines.filter((x) => x.stream === "stderr");
        const source = stderr.length ? stderr : r.data.lines;
        const tail = source.slice(0, 5).reverse();
        const prev = logTails.get(row.id);
        const changed = !prev || prev.length !== tail.length
          || prev.some((p, i) => p.ts !== tail[i].ts || p.line !== tail[i].line);
        logTails.set(row.id, tail);
        if (changed) fetched = true;
      } finally {
        tailsFetching.delete(row.id);
      }
    }
    // Repaint only if these are still the current rows (a reload or an
    // error paint may have superseded them while fetches were in flight).
    if (fetched && !disposed && lastRows === rows) paint();
  };

  const load = async (): Promise<void> => {
    const seq = ++loadSeq;
    const [r, mr, gr, pr, tr] = await Promise.all([getBackends(), getModels(), getGatewayConfig(), getActivationPlan(), getGpuTelemetry()]);
    if (disposed || seq !== loadSeq) return; // a newer load supersedes this one
    if (!pr.error && pr.data) { lastPlan = pr.data; paintPlan(); }
    if (!tr.error && tr.data) {
      lastGpuList = tr.data.gpus;
      const m = new Map<string, { gpu: number; name: string; mib: number | null }[]>();
      for (const g of tr.data.gpus) {
        for (const p of g.processes ?? []) {
          if (p.backend === "\u2014") continue;
          const arr = m.get(p.backend) ?? [];
          arr.push({ gpu: Number(g.id), name: g.name, mib: p.vram_mib });
          m.set(p.backend, arr);
        }
      }
      residency = m;
    }
    if (!mr.error && mr.data) lastModels = mr.data;
    if (!gr.error && gr.data) {
      lastGroups = gr.data.groups;
      if (gr.data.port) gatewayPort = gr.data.port;
    }
    setBanner(root, "backBanner", bannerHtml(r));
    const b = root.querySelector("#backBody");
    if (!b) return;
    if (r.error) { lastRows = []; b.innerHTML = emptyHtml(`Backends unavailable: ${r.error}`); return; }
    if (!r.data.length) {
      lastRows = [];
      b.innerHTML = emptyHtml(r.live
        ? "No backends are configured. Backends are defined in the app configuration."
        : "No backend data in sample mode.");
      return;
    }
    // Note/arming lifetimes key to the SERVER state (not the local
    // pending overlay), so a lifecycle action's own overlay does not
    // clear the note it produces.
    const prevStates = new Map(wireRows.map((row) => [row.id, fromWire(row.lifecycle ?? row.state)]));
    wireRows = r.data;
    lastRows = wireRows.map(effRow);
    // Bench adoption path (Bench design §5.2): an arm detail's
    // "Open in backend editor" sets this key before navigating
    // here; open that backend's flags form once, prefilled from
    // its saved configuration, and clear the key.
    try {
      const wanted = sessionStorage.getItem("benchEditBackend");
      if (wanted) {
        sessionStorage.removeItem("benchEditBackend");
        const row = lastRows.find((r) => r.id === wanted);
        if (row && flagsFor !== row.id) openFlagsForm(row);
      }
    } catch { /* storage unavailable */ }
    for (const row of lastRows) {
      // A note belongs to the state it was produced in; when the
      // backend's state changes, the stale note goes away.
      const prev = prevStates.get(row.id);
      const wireNow = fromWire(wireRows.find((w) => w.id === row.id)?.lifecycle ?? row.state);
      if (prev !== undefined && prev !== wireNow) { msgs.delete(row.id); confirmRemove.delete(row.id); confirmOverride.delete(row.id); armedWddmStart.delete(row.id); }
      // A config change that ends the paged fit disarms the
      // confirmation with it.
      if (row.fit?.state !== "wddm_paged") armedWddmStart.delete(row.id);
      // A backend that is no longer failed gets a fresh tail fetch if it
      // fails again — unless its Details are open, where the stored
      // tail is the expansion's latest-lines data and stays.
      if (row.state !== "failed" && !openDetails.has(row.id)) { logTails.delete(row.id); tailsWanted.delete(row.id); }
    }
    // Prune per-backend card state for ids that no longer exist
    // (code-review 5.9): a deleted backend's notes, caches, and
    // open forms must not linger — or resurface if the id is ever
    // reused.
    {
      const alive = new Set(lastRows.map((row) => row.id));
      for (const map of [msgs, argvCache, logTails] as Map<string, unknown>[]) {
        for (const k of [...map.keys()]) if (!alive.has(k)) map.delete(k);
      }
      for (const set of [confirmRemove, confirmOverride, armedWddmStart, openArgv, openDetails, inFlight, tailsWanted, tailsFetching] as Set<string>[]) {
        for (const k of [...set]) if (!alive.has(k)) set.delete(k);
      }
      if (flagsFor !== null && !alive.has(flagsFor)) flagsFor = null;
      if (offloadPending !== null && !alive.has(offloadPending.backendId)) offloadPending = null;
      if (placeFor !== null && !alive.has(placeFor)) { placeFor = null; placeProjection = null; }
    }
    // While the offload slider is focused (mid-drag), a repaint
    // would recreate the range input and break the drag; the
    // draft is synced to wire state, so skipping one repaint
    // loses nothing (same rule as the model rename input). The
    // advanced flags textarea (tracker #60) follows the same
    // rule: recreating it mid-edit would lose the caret.
    const sliderFocused = flagsFor !== null
      && (document.activeElement as HTMLElement | null)?.hasAttribute?.("data-offload-slider");
    const rawFocused = flagsFor !== null
      && (document.activeElement as HTMLElement | null)?.hasAttribute?.("data-flags-raw");
    if (!sliderFocused && !rawFocused) paint();
    void fetchLogTails();
  };

  const refreshBtn = root.querySelector("#backRefresh");
  const onRefresh = (): void => { void load(); };
  refreshBtn?.addEventListener("click", onRefresh);

  /* ---------------- Flags editor ----------------
   * Rendered INSIDE the backend's own card (backendCard, from the
   * draft state below) — the previous placement above the list read
   * as if nothing had happened when Edit was clicked. The
   * draft values live outside the repainted DOM, so the poll repaint
   * cannot clobber an open form: paint() re-renders the form from
   * this state, and field events sync back into it. */
  let flagsFor: string | null = null;
  let flagsCtx = 32768;
  let flagsBatch = 512;
  /** The Edit panel's model draft: the registered model chosen
   *  in the panel's Model select ("" when the open backend's file
   *  matches no registered model). Synced on the select's change
   *  event and applied through model_assign only when the panel
   *  saves; closing the panel discards it. */
  let flagsEditModelId = "";
  /** Offload slider draft position for the open flags form (MoE
   *  layers on system RAM); null when the backend's model is not
   *  offload-capable. */
  let flagsOffloadLayers: number | null = null;
  /** A released offload position the guardrail blocks, awaiting the
   *  operator's decision in the flags form (save with override, or
   *  snap to the nearest verified position); null otherwise. */
  let offloadPending: { backendId: string; layers: number } | null = null;
  /** True while a slider commit (backend_set_offload) is in
   *  flight; the slider ignores further commits until it lands. */
  let offloadCommitting = false;
  /** The MTP draft control's chosen value while the flags form
   *  is open (0 = Off, 1..4 = draft tokens); null when the open
   *  backend's model is not MTP-capable. */
  let flagsMtpPosition: number | null = null;
  /** True while an MTP draft commit (backend_set_mtp) is in
   *  flight; the control ignores further commits until it
   *  lands. */
  let mtpCommitting = false;
  /** The expert-cache control's draft while the flags form is
   *  open: the switch state, and the chosen size — retained
   *  while the switch is off so toggling back on restores it;
   *  null when the backend has never carried a cache and none
   *  has been chosen. */
  let flagsMoeCacheOn = false;
  let flagsMoeCacheMib: number | null = null;
  /** True while an expert-cache commit (backend_set_moe_cache)
   *  is in flight; the control ignores further commits until it
   *  lands. */
  let moeCacheCommitting = false;
  /** The Parallel slots field's draft selection while the flags
   *  form is open: the explicit --parallel count, or null for the
   *  unset state (llama.cpp's default, auto). Unlike the MTP
   *  position this exists for every backend — the field is not
   *  capability-gated. */
  let flagsParallel: number | null = null;
  /** True while a parallel commit (backend_set_parallel) is in
   *  flight; the field ignores further commits until it lands. */
  let parallelCommitting = false;
  /** The KV cache / flash-attention draft selection while the
   *  flags form is open (the decoded saved selection at open);
   *  commits through backend_set_cache on change, like MTP. */
  let flagsCacheSel: CacheSelectionLike | null = null;
  /** The last cache preview priced for the current draft
   *  (predicted footprint + predicted-best ladder), held in wire
   *  state so poll repaints re-render it. */
  let flagsCachePreview: CachePreview | null = null;
  let cacheCommitting = false;
  let cachePreviewSeq = 0;
  let cachePreviewTimer: number | null = null;
  /** The advanced flags editor's draft text (tracker #60): the
   *  open backend's raw flags as edited, synced on every input
   *  event so poll repaints re-render the textarea from here.
   *  Initialized from the row's saved string when the form opens —
   *  opening the editor performs no write. */
  let flagsRawText = "";
  /** The draft's validation warnings, recomputed live as the
   *  operator types (the save op's own list replaces them after a
   *  save — the server is the authority). */
  let flagsRawWarnings: FlagWarningLike[] = [];
  /** The open flags form's predicted-throughput line
   *  (predicted-throughput design §6): the last lookup's
   *  rendered line for the CURRENT draft, held in wire state
   *  so poll repaints re-render it and a lookup in flight
   *  never blanks it (§6.2). Empty when the draft matches no
   *  measurement exactly or nearly. */
  let predictedHtml = "";
  let predictedSeq = 0;
  let predictedTimer: number | null = null;
  const runPredictedLookup = async (): Promise<void> => {
    const id = flagsFor;
    if (!id) return;
    const row = lastRows.find((r) => r.id === id);
    if (!row) return;
    const seq = ++predictedSeq;
    const req: MeasurementLookupRequest = row.flags_advanced
      ? { backend_id: id, raw_flags: flagsRawText }
      : {
          backend_id: id,
          n_ctx: flagsCtx,
          n_batch: flagsBatch,
          offload_layers: flagsOffloadLayers,
          mtp_position: flagsMtpPosition,
          parallel_slots: flagsParallel,
        };
    const r = await measurementLookup(req);
    if (disposed || seq !== predictedSeq || flagsFor !== id) return;
    predictedHtml = r.data ? predictedLineHtml(r.data.exact, r.data.near) : "";
    // Update the open form's slot in place: a repaint here
    // could disturb an in-progress slider drag or text edit.
    const slot = root.querySelector("[data-predicted-slot]");
    if (slot) slot.innerHTML = predictedHtml;
  };
  /** Debounced lookup trigger (§6.2, ~300 ms): fires on the
   *  same events that mutate the draft. */
  const schedulePredictedLookup = (): void => {
    if (predictedTimer !== null) window.clearTimeout(predictedTimer);
    predictedTimer = window.setTimeout(() => {
      predictedTimer = null;
      void runPredictedLookup();
    }, 300);
  };
  const runCachePreview = async (): Promise<void> => {
    const id = flagsFor;
    if (!id || !flagsCacheSel) return;
    const seq = ++cachePreviewSeq;
    // The Expert cache control's DRAFT prices with the preview
    // (its saved token does not change until the control's own
    // commit lands, and a cache change alone must move the fit
    // figures immediately — Pete's Oct 23, 2026 report).
    const moeDraft = flagsMoeCacheOn
      ? (flagsMoeCacheMib ?? MOE_CACHE_DEFAULT_MIB)
      : 0;
    const r = await backendCachePreview(id, { nCtx: flagsCtx, slots: flagsParallel ?? undefined, sel: flagsCacheSel, moeCacheMib: moeDraft });
    if (disposed || seq !== cachePreviewSeq || flagsFor !== id || r.error || !r.data) return;
    flagsCachePreview = r.data;
    const slot = root.querySelector("[data-cache-slot]");
    if (slot && flagsCacheSel) slot.innerHTML = cachePreviewHtml(flagsCachePreview, flagsCacheSel, `data-cache-best data-cache-backend="${esc(id)}"`);
  };
  const scheduleCachePreview = (): void => {
    if (cachePreviewTimer !== null) window.clearTimeout(cachePreviewTimer);
    cachePreviewTimer = window.setTimeout(() => {
      cachePreviewTimer = null;
      void runCachePreview();
    }, 300);
  };
  const closeFlagsForm = (): void => {
    flagsFor = null;
    offloadPending = null;
    flagsMoeCacheOn = false;
    flagsMoeCacheMib = null;
    flagsCacheSel = null;
    flagsCachePreview = null;
    cachePreviewSeq += 1;
    if (cachePreviewTimer !== null) { window.clearTimeout(cachePreviewTimer); cachePreviewTimer = null; }
    predictedHtml = "";
    predictedSeq += 1;
    if (predictedTimer !== null) { window.clearTimeout(predictedTimer); predictedTimer = null; }
    paint();
  };
  const openFlagsForm = (row: BackendRow): void => {
    if (flagsFor === row.id) { closeFlagsForm(); return; }
    flagsFor = row.id;
    flagsEditModelId = lastModels.find((m) => sameModelFile(row.model_file, m.gguf_path))?.id ?? "";
    flagsCtx = row.n_ctx ?? 32768;
    flagsBatch = row.n_batch ?? 512;
    flagsOffloadLayers = row.offload ? row.offload.layers : null;
    flagsMtpPosition = row.mtp ? mtpPosition(row.mtp) : null;
    flagsMoeCacheOn = row.moe_cache ? row.moe_cache.mib != null : false;
    flagsMoeCacheMib = row.moe_cache ? row.moe_cache.mib : null;
    flagsParallel = row.parallel ? row.parallel.slots : null;
    flagsCacheSel = row.cache ? { k: row.cache.k, v: row.cache.v, flash_attn: row.cache.flash_attn } : { k: "f16", v: "f16", flash_attn: "auto" };
    flagsCachePreview = null;
    offloadPending = null;
    // The advanced editor opens on the backend's CURRENT saved
    // raw flags — a pure read of the row payload; opening writes
    // nothing (tracker #60, entry performs no write).
    flagsRawText = row.raw_flags ?? "";
    flagsRawWarnings = rawFlagWarnings(flagsRawText);
    predictedHtml = "";
    predictedSeq += 1;
    paint();
    schedulePredictedLookup();
    scheduleCachePreview();
  };
  /** Keep the draft in step with the in-card form fields. Preset
   *  selects repaint (to show or hide the Custom numeric input);
   *  custom inputs only update the draft. Registered for both
   *  "change" and "input" so typing is captured before any repaint. */
  const syncFlagsDraft = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    const field = t.getAttribute("data-flags-field");
    if (!field) return;
    const draft = { ctx: flagsCtx, batch: flagsBatch, parallel: flagsParallel };
    const repaint = applyFlagsField(draft, field, (t as HTMLInputElement).value, ev.type === "change");
    flagsCtx = draft.ctx;
    flagsBatch = draft.batch;
    flagsParallel = draft.parallel;
    if (repaint) paint();
    schedulePredictedLookup();
    scheduleCachePreview();
  };
  root.addEventListener("change", syncFlagsDraft);
  root.addEventListener("input", syncFlagsDraft);
  /** The Edit panel's Model select sync: the choice is a DRAFT,
   *  recorded here and applied through model_assign only when
   *  the panel saves (Pete's report, 2026-10-20 — the card-face
   *  dropdown this replaces assigned on selection). No repaint:
   *  the select already shows the chosen option, and the draft
   *  re-renders it on the next poll repaint. */
  const syncEditModel = (ev: Event): void => {
    const t = ev.target as HTMLSelectElement;
    if (!t.hasAttribute("data-edit-model")) return;
    if (t.getAttribute("data-edit-model") !== flagsFor) return;
    flagsEditModelId = t.value;
  };
  root.addEventListener("change", syncEditModel);
  /** The advanced editor's textarea sync (tracker #60): keep the
   *  draft text and its live warnings in wire state. The warning
   *  list updates in place — no repaint, so typing is never
   *  disturbed (the poll repaint is also skipped while the
   *  textarea is focused; see load()). */
  const syncRawDraft = (ev: Event): void => {
    const t = ev.target as HTMLTextAreaElement;
    if (!t.hasAttribute("data-flags-raw")) return;
    if (t.getAttribute("data-flags-raw") !== flagsFor) return;
    flagsRawText = t.value;
    flagsRawWarnings = rawFlagWarnings(flagsRawText);
    const box = t.closest(".backend-card")?.querySelector("[data-flags-warnings]");
    if (box) box.innerHTML = flagsRawWarningsHtml(flagsRawWarnings);
    schedulePredictedLookup();
  };
  root.addEventListener("input", syncRawDraft);

  /* ---------------- Offload slider ----------------
   * Part of the flags form when the backend's model is
   * offload-capable. "input" events move the draft and update the
   * readout in place — no repaint, so the drag is never disturbed;
   * the "change" event (slider release) commits through
   * backend_set_offload, which rewrites only the offload tokens
   * in the backend's raw flags. A rejected commit reverts the
   * draft to the saved position and keeps the form open with the
   * error on the card. */
  const syncOffloadDraft = (ev: Event): void => {
    const t = ev.target as HTMLInputElement;
    if (!t.hasAttribute("data-offload-slider")) return;
    const id = t.getAttribute("data-offload-backend") ?? "";
    if (id !== flagsFor) return;
    const row = lastRows.find((r) => r.id === id);
    if (!row?.offload) return;
    const n = Number(t.value);
    flagsOffloadLayers = n;
    schedulePredictedLookup();
    const card = t.closest(".backend-card");
    if (!card) return;
    const o = row.offload;
    paintOffloadReadout(card, o, n);
    // Keep a pending guardrail decision in step with the draft:
    // its text follows a still-blocked position, and the row hides
    // while the draft sits on a verified position (the release
    // handler re-decides from the final position).
    if (offloadPending?.backendId === id) {
      const dec = card.querySelector("[data-offload-decision]") as HTMLElement | null;
      if (dec) {
        if (offloadIsUntested(o, n)) {
          offloadPending.layers = n;
          dec.style.display = "";
          const dt = dec.querySelector("[data-offload-decision-text]");
          if (dt) dt.textContent = `${n} of ${o.moe_layers} layers is unverified on this GPU`;
        } else {
          dec.style.display = "none";
        }
      }
    }
  };
  /** Invoke backend_set_offload for one position and report the
   *  outcome on the card. An override save's message names the
   *  override; any other save keeps the token-form message. On
   *  error the message carries the server's error and the pending
   *  decision (if any) is left for the caller to restore. Returns
   *  true when the position was saved. */
  const saveOffloadPosition = async (id: string, n: number, allowOverride: boolean): Promise<boolean> => {
    if (offloadCommitting) return false;
    const row = lastRows.find((r) => r.id === id);
    offloadCommitting = true;
    inFlight.add(id);
    paint();
    try {
      const r = await setBackendOffload(id, n, allowOverride);
      if (disposed) return false;
      if (r.error) {
        msgs.set(id, { text: `Offload not saved: ${r.error}`, err: true });
        paint();
        return false;
      }
      offloadPending = null;
      if (flagsFor === id) flagsOffloadLayers = n;
      msgs.set(id, {
        text: allowOverride
          ? "Offload saved with override \u2014 applies on next start."
          : `Offload saved (${row?.offload ? offloadTokenText(row.offload, n) : n}) \u2014 applies on next start.`,
        err: false,
      });
      await load();
      schedulePredictedLookup();
      return true;
    } finally {
      offloadCommitting = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const commitOffload = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLInputElement;
    if (!t.hasAttribute("data-offload-slider")) return;
    const id = t.getAttribute("data-offload-backend") ?? "";
    const row = lastRows.find((r) => r.id === id);
    if (!row?.offload || offloadCommitting) return;
    const n = Number(t.value);
    flagsOffloadLayers = n;
    if (n === row.offload.layers) { offloadPending = null; paint(); return; } // released where it started
    // A guardrail-blocked position is NOT invoked: the form renders
    // the override/snap decision row for it instead.
    if (offloadIsUntested(row.offload, n)) {
      offloadPending = { backendId: id, layers: n };
      paint();
      return;
    }
    const ok = await saveOffloadPosition(id, n, false);
    if (!ok && !disposed) {
      // A rejected commit reverts the draft to the saved position.
      flagsOffloadLayers = row.offload.layers;
      paint();
    }
  };
  root.addEventListener("input", syncOffloadDraft);
  root.addEventListener("change", commitOffload);

  /* ---------------- MTP draft control (tracker #53) ----------------
   * A dropdown (the ctx/batch selects' idiom), so there is no
   * drag preview to sync: the "change" event does the whole
   * job — it moves the draft, updates the readouts in place,
   * and commits through backend_set_mtp, which rewrites only
   * the MTP tokens in the backend's raw flags. Value 0 commits
   * as Off. Choosing the saved value is a no-op. A rejected
   * commit reverts the draft to the saved value. The
   * --parallel conflict line follows the draft: it shows
   * whenever the value enables MTP and the SAVED flags set a
   * slot count other than 1 — the control never rewrites that
   * setting. */
  /** Invoke backend_set_mtp for one value and report the
   *  outcome on the card. Returns true when the value was
   *  saved. */
  const saveMtpPosition = async (id: string, pos: number): Promise<boolean> => {
    if (mtpCommitting) return false;
    mtpCommitting = true;
    inFlight.add(id);
    paint();
    try {
      const r = await setBackendMtp(id, pos === 0 ? null : pos);
      if (disposed) return false;
      if (r.error) {
        msgs.set(id, { text: `MTP draft not saved: ${r.error}`, err: true });
        paint();
        return false;
      }
      if (flagsFor === id) flagsMtpPosition = pos;
      msgs.set(id, {
        text: pos === 0
          ? "MTP draft turned off — applies on next start."
          : `MTP draft saved (${mtpTokenText(pos)}) — applies on next start.`,
        err: false,
      });
      await load();
      schedulePredictedLookup();
      return true;
    } finally {
      mtpCommitting = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const changeMtp = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLSelectElement;
    if (!t.hasAttribute("data-mtp-select")) return;
    const id = t.getAttribute("data-mtp-backend") ?? "";
    const row = lastRows.find((r) => r.id === id);
    if (!row?.mtp || mtpCommitting) return;
    const n = Number(t.value);
    flagsMtpPosition = n;
    schedulePredictedLookup();
    const card = t.closest(".backend-card");
    if (card) {
      const token = card.querySelector("[data-mtp-token]");
      if (token) token.textContent = mtpTokenText(n);
      const conflict = card.querySelector("[data-mtp-conflict]") as HTMLElement | null;
      if (conflict) {
        const show = n > 0 && row.mtp.parallel != null && row.mtp.parallel !== 1;
        conflict.style.display = show ? "" : "none";
      }
    }
    if (n === mtpPosition(row.mtp)) { paint(); return; } // the saved value chosen again
    const ok = await saveMtpPosition(id, n);
    if (!ok && !disposed) {
      // A rejected commit reverts the draft to the saved value.
      flagsMtpPosition = mtpPosition(row.mtp);
      paint();
    }
  };
  root.addEventListener("change", changeMtp);

  /* ---------------- Expert cache control ----------------
   * The switch commits immediately (the sliders' commit model):
   * on commits the drafted size (the saved size when one exists,
   * else the control's default); off commits null, and the size
   * draft is retained so toggling back on restores it. The size
   * select only drafts while the switch is off; while on, a
   * change commits the new size, and "Auto" commits with auto
   * set — the server resolves the size from priced free VRAM and
   * the draft takes the resolved value. A rejected commit
   * reverts the draft to the saved state. */
  /** Invoke backend_set_moe_cache and report the outcome on the
   *  card. Returns true when the value was saved. */
  const saveMoeCache = async (id: string, mib: number | null, auto: boolean): Promise<boolean> => {
    if (moeCacheCommitting) return false;
    moeCacheCommitting = true;
    inFlight.add(id);
    paint();
    try {
      const r = await setBackendMoeCache(id, mib, auto || undefined);
      if (disposed) return false;
      if (r.error) {
        msgs.set(id, { text: `Expert cache not saved: ${r.error}`, err: true });
        paint();
        return false;
      }
      const saved = r.data.mib;
      if (flagsFor === id) {
        flagsMoeCacheOn = saved != null;
        if (saved != null) flagsMoeCacheMib = saved;
      }
      msgs.set(id, {
        text: saved == null
          ? "Expert cache turned off — applies on next start."
          : auto
            ? `Expert cache saved (${moeCacheTokenText(saved)}) — size resolved by Auto from priced free VRAM; applies on next start.`
            : `Expert cache saved (${moeCacheTokenText(saved)}) — applies on next start.`,
        err: false,
      });
      await load();
      // A cache change re-prices the flags fit block at once —
      // toggling or resizing the Expert cache must move the fit
      // figures by itself, not only when an unrelated field
      // happens to re-trigger a preview (Pete, Oct 23, 2026).
      scheduleCachePreview();
      return true;
    } finally {
      moeCacheCommitting = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const revertMoeCacheDraft = (row: BackendRow): void => {
    flagsMoeCacheOn = row.moe_cache ? row.moe_cache.mib != null : false;
    flagsMoeCacheMib = row.moe_cache ? row.moe_cache.mib : null;
  };
  const clickMoeCacheSwitch = async (ev: Event): Promise<void> => {
    const t = (ev.target as HTMLElement).closest("[data-moe-cache-switch]") as HTMLElement | null;
    if (!t) return;
    const id = t.getAttribute("data-moe-cache-backend") ?? "";
    const row = lastRows.find((r) => r.id === id);
    if (!row?.moe_cache || moeCacheCommitting) return;
    const on = !flagsMoeCacheOn;
    flagsMoeCacheOn = on;
    // The toggle alone re-prices the fit block: the preview
    // carries the control's draft, so the figures move before
    // the save even lands.
    scheduleCachePreview();
    const card = t.closest(".backend-card");
    const token = card?.querySelector("[data-moe-cache-token]");
    if (token) token.textContent = moeCacheTokenText(on ? (flagsMoeCacheMib ?? row.moe_cache.mib ?? MOE_CACHE_DEFAULT_MIB) : null);
    const ok = await saveMoeCache(id, on ? (flagsMoeCacheMib ?? MOE_CACHE_DEFAULT_MIB) : null, false);
    if (!ok && !disposed) {
      revertMoeCacheDraft(row);
      paint();
    }
  };
  root.addEventListener("click", clickMoeCacheSwitch);
  const changeMoeCacheSize = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLSelectElement;
    if (!t.hasAttribute("data-moe-cache-select")) return;
    const id = t.getAttribute("data-moe-cache-backend") ?? "";
    const row = lastRows.find((r) => r.id === id);
    if (!row?.moe_cache || moeCacheCommitting) return;
    if (t.value === "auto") {
      const ok = await saveMoeCache(id, null, true);
      if (!ok && !disposed) {
        revertMoeCacheDraft(row);
        paint();
      }
      return;
    }
    const n = Number(t.value);
    flagsMoeCacheMib = n;
    scheduleCachePreview();
    if (!flagsMoeCacheOn) { paint(); return; } // the size only drafts while off
    const card = t.closest(".backend-card");
    const token = card?.querySelector("[data-moe-cache-token]");
    if (token) token.textContent = moeCacheTokenText(n);
    if (n === row.moe_cache.mib) { paint(); return; } // the saved value chosen again
    const ok = await saveMoeCache(id, n, false);
    if (!ok && !disposed) {
      revertMoeCacheDraft(row);
      paint();
    }
  };
  root.addEventListener("change", changeMoeCacheSize);

  /* ---------------- Parallel slots field (tracker #61) --------
   * The ctx/batch idiom with the sliders' commit model: the
   * draft syncs through syncFlagsDraft (the shared collection
   * logic), and a "change" on the preset select or the Custom
   * input commits through backend_set_parallel, which rewrites
   * only the --parallel tokens in the backend's raw flags.
   * Choosing Custom… only reveals the numeric input — the value
   * commits when the input itself changes. A rejected or invalid
   * commit reverts the draft to the saved selection. Setting the
   * field to 1 is also what clears the MTP conflict line: the
   * reload after the commit refreshes the payload it renders
   * from. */
  const saveParallelValue = async (id: string, n: number | null): Promise<boolean> => {
    if (parallelCommitting) return false;
    parallelCommitting = true;
    inFlight.add(id);
    paint();
    try {
      const r = await setBackendParallel(id, n);
      if (disposed) return false;
      if (r.error) {
        msgs.set(id, { text: `Parallel slots not saved: ${r.error}`, err: true });
        paint();
        return false;
      }
      if (flagsFor === id) flagsParallel = n;
      msgs.set(id, {
        text: n === null
          ? "Parallel slots returned to the llama.cpp default (auto) \u2014 applies on next start."
          : `Parallel slots saved (${parallelTokenText(n)}) \u2014 applies on next start.`,
        err: false,
      });
      await load();
      schedulePredictedLookup();
      return true;
    } finally {
      parallelCommitting = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const commitParallel = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;
    const field = t.getAttribute("data-flags-field");
    if (field !== "parallel-preset" && field !== "parallel-custom") return;
    if (!flagsFor || parallelCommitting) return;
    // Choosing Custom… reveals the numeric input; nothing commits
    // until the input itself changes.
    if (field === "parallel-preset" && (t as HTMLSelectElement).value === "custom") return;
    const id = flagsFor;
    const row = lastRows.find((r) => r.id === id);
    if (!row?.parallel) return;
    const saved = row.parallel.slots;
    const n = flagsParallel;
    if (n === saved) { paint(); return; } // changed back to the saved selection
    // The same bounds the set op validates, checked here so the
    // error reads in the form's own terms; the op re-validates
    // regardless.
    if (n !== null && (!Number.isInteger(n) || n < PARALLEL_MIN || n > PARALLEL_MAX)) {
      msgs.set(id, { text: `Parallel slots must be between ${PARALLEL_MIN} and ${fmtCount(PARALLEL_MAX)}.`, err: true });
      flagsParallel = saved;
      paint();
      return;
    }
    const ok = await saveParallelValue(id, n);
    if (!ok && !disposed) {
      // A rejected commit reverts the draft to the saved selection.
      flagsParallel = saved;
      paint();
    }
  };
  root.addEventListener("change", commitParallel);

  /* ---------------- KV cache / flash attention ----------------
   * Three selects (the MTP idiom): a "change" on any of them
   * builds the full selection from the card's selects, moves the
   * draft, refreshes the preview, and commits through
   * backend_set_cache, which rewrites only the cache tokens in
   * the backend's raw flags. The quantized-V + FA-off pairing is
   * named in the form before committing (the op refuses it too).
   * Choosing the saved selection is a no-op; a rejected commit
   * reverts the draft to the saved selection. The predicted-best
   * button applies the preview's best rung through the same
   * commit path — nothing is ever applied without that click. */
  const saveCacheSelection = async (id: string, sel: CacheSelectionLike): Promise<boolean> => {
    if (cacheCommitting) return false;
    cacheCommitting = true;
    inFlight.add(id);
    paint();
    try {
      const r = await setBackendCache(id, sel);
      if (disposed) return false;
      if (r.error) {
        msgs.set(id, { text: `KV cache not saved: ${r.error}`, err: true });
        paint();
        return false;
      }
      if (flagsFor === id) flagsCacheSel = sel;
      msgs.set(id, { text: `KV cache saved (${cacheTokenText(sel)}) — applies on next start.`, err: false });
      await load();
      schedulePredictedLookup();
      scheduleCachePreview();
      return true;
    } finally {
      cacheCommitting = false;
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const commitCacheSelection = async (id: string, sel: CacheSelectionLike): Promise<void> => {
    const row = lastRows.find((r) => r.id === id);
    if (!row?.cache || cacheCommitting) return;
    flagsCacheSel = sel;
    scheduleCachePreview();
    if (cacheVQuantizedFaOff(sel)) {
      msgs.set(id, { text: "A quantized V cache requires flash attention: choose flash attention auto or on.", err: true });
      flagsCacheSel = { k: row.cache.k, v: row.cache.v, flash_attn: row.cache.flash_attn };
      paint();
      return;
    }
    const saved: CacheSelectionLike = { k: row.cache.k, v: row.cache.v, flash_attn: row.cache.flash_attn };
    if (row.cache.present && sameCacheSelection(sel, saved)) { paint(); return; }
    const ok = await saveCacheSelection(id, sel);
    if (!ok && !disposed) {
      flagsCacheSel = saved;
      paint();
    }
  };
  const changeCache = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLSelectElement;
    if (!t.hasAttribute("data-cache-field")) return;
    if (!flagsFor) return;
    const card = t.closest(".backend-card");
    if (!card) return;
    const read = (field: string): string => (card.querySelector(`[data-cache-field="${field}"]`) as HTMLSelectElement | null)?.value ?? "";
    const sel: CacheSelectionLike = {
      k: read("cache-k") as CacheTypeName,
      v: read("cache-v") as CacheTypeName,
      flash_attn: read("flash-attn") as FlashAttnName,
    };
    if (!CACHE_TYPES.includes(sel.k) || !CACHE_TYPES.includes(sel.v) || !FLASH_ATTN_MODES.includes(sel.flash_attn)) return;
    await commitCacheSelection(flagsFor, sel);
  };
  root.addEventListener("change", changeCache);
  const clickCacheBest = async (ev: Event): Promise<void> => {
    const t = (ev.target as HTMLElement).closest("[data-cache-best]") as HTMLElement | null;
    if (!t) return;
    const id = t.getAttribute("data-cache-backend") ?? "";
    if (!id || id !== flagsFor || !flagsCachePreview?.best) return;
    const best = flagsCachePreview.best;
    await commitCacheSelection(id, { k: best.k, v: best.v, flash_attn: best.flash_attn });
  };
  root.addEventListener("click", clickCacheBest);

  /** Apply the Edit panel's model draft through model_assign,
   *  carrying the backend's current tensor split so the
   *  assignment changes the model only (the split is the
   *  Placement section's own save). Returns "none" when the
   *  draft names the model the backend already serves (or no
   *  registered model), "saved" when the assignment landed, and
   *  "failed" when the op rejected it — the error is on the
   *  card and the caller keeps the panel open, saving nothing
   *  else, so a failed model change never reads as a partial
   *  save. model_assign itself is atomic (it commits only after
   *  placement, validation, journal, and save all succeed) and
   *  reports flags_untouched for advanced-flags backends. */
  const saveEditModel = async (id: string): Promise<"none" | "saved" | "failed"> => {
    const row = lastRows.find((r) => r.id === id);
    const model = lastModels.find((m) => m.id === flagsEditModelId);
    if (!row || !model || sameModelFile(row.model_file, model.gguf_path)) return "none";
    const cfgR = await getBackendConfig(id);
    const cfgFlags = cfgR.data?.flags as { tensor_split?: unknown } | undefined;
    const split = Array.isArray(cfgFlags?.tensor_split)
      && cfgFlags.tensor_split.every((x) => typeof x === "number" && Number.isFinite(x))
      ? (cfgFlags.tensor_split as number[]) : [];
    const r = await modelAssign(id, model.id, split);
    if (disposed) return "failed";
    if (r.error) {
      msgs.set(id, { text: `Model change failed: ${r.error}`, err: true });
      return "failed";
    }
    return "saved";
  };

  /* ---------------- Placement form ----------------
   * Rendered INSIDE the backend's own card (backendCard), like the
   * flags editor; the GPUs page shows placement read-only. State
   * lives outside the repainted DOM: which backend's form is open,
   * the unchecked GPU indices, and the last projection. Checkbox
   * changes sync into the state and invalidate the projection (it
   * no longer matches the selection). */
  let placeFor: string | null = null;
  const placeExcluded = new Set<number>();
  let placeProjection: { backendId: string; modelId: string; data: PlacementProjectionData } | null = null;
  let placeBusy = false;
  /** Latest telemetry GPU list (checkbox source), refreshed by load(). */
  let lastGpuList: GpuInfo[] = [];
  const closePlaceForm = (): void => {
    placeFor = null;
    placeProjection = null;
    placeExcluded.clear();
    paint();
  };
  /** Close the whole Edit session (the Edit panel and the
   *  Placement section). A successful Save closes the surface,
   *  returning the card to its normal state (Pete's report,
   *  2026-10-20: Save flags left the edit form open — most
   *  visibly the advanced editor, whose save deliberately kept
   *  the panel up). A failed save never calls this: the surface
   *  stays open with the operator's input intact and the error
   *  on the card. */
  const closeEditSession = (): void => { closeFlagsForm(); closePlaceForm(); };
  const openPlaceForm = (row: BackendRow): void => {
    if (placeFor === row.id) { closePlaceForm(); return; }
    placeFor = row.id;
    placeProjection = null;
    placeExcluded.clear();
    // Default the selection to the backend's CURRENT placement, not
    // every GPU: the configured tensor split's positive entries when
    // a split is configured; otherwise the GPUs the loaded process
    // measurably holds weights on (>= 1 GiB — under that is a bare
    // CUDA context); otherwise, for an unsplit backend that is not
    // loaded, the first GPU, which is where an unsplit launch loads.
    const allIdx = lastGpuList.map((g) => Number(g.id));
    const cfgIdx = (row.tensor_split ?? [])
      .map((f, p) => (f > 0 && p < allIdx.length ? allIdx[p] : -1))
      .filter((i) => i >= 0);
    let current = cfgIdx;
    if (!current.length) {
      // Unmeasured rows (mib null) never select a GPU: unknown usage
      // is not residency. The default then falls back to the first
      // GPU, as for an unloaded backend.
      const resIdx = (residency.get(row.id) ?? [])
        .filter((r) => r.mib !== null && r.mib >= 1024)
        .map((r) => r.gpu)
        .filter((i) => allIdx.includes(i));
      current = resIdx.length ? resIdx : allIdx.slice(0, 1);
    }
    for (const ix of allIdx) if (!current.includes(ix)) placeExcluded.add(ix);
    paint();
  };
  const syncPlaceDraft = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    if (!t.hasAttribute("data-place-gpu")) return;
    if (t.getAttribute("data-place-backend") !== placeFor) return;
    const ix = Number(t.getAttribute("data-place-gpu"));
    if ((t as HTMLInputElement).checked) placeExcluded.delete(ix);
    else placeExcluded.add(ix);
    placeProjection = null;
    paint();
  };
  root.addEventListener("change", syncPlaceDraft);

  /* ---------------- Add backend form ----------------
   * Lives in #backAddForm, outside the repainted #backBody, so the
   * 5-second poll never clobbers typed input. */
  const addHost = root.querySelector("#backAddForm");
  const closeAddForm = (): void => { if (addHost) addHost.innerHTML = ""; };
  const nextFreePort = (): number => {
    const used = new Set<number>(lastRows.map((r) => r.port));
    used.add(gatewayPort);
    used.add(8090); // the Windows ML backend's conventional port
    let p = 8081;
    while (used.has(p)) p += 1;
    return p;
  };
  const openAddForm = (): void => {
    if (!addHost) return;
    if (addHost.innerHTML) { closeAddForm(); return; }
    // No-models state: a compact directive card — no form, no
    // disabled submit, no prose (principle 13).
    if (!lastModels.length) {
      addHost.innerHTML = `<div class="card"><h3>${icon("server")} Add a backend</h3>
        <div class="kv" style="margin-top:10px"><span class="k">Models</span><span class="v" style="font-size:12px">none registered</span></div>
        <div class="card-actions"><a class="btn" href="#/models">Add a model</a>
          <button class="btn icon-only" id="addBackCancel" title="Cancel" aria-label="Cancel">${icon("x")}</button></div></div>`;
      addHost.querySelector("#addBackCancel")?.addEventListener("click", closeAddForm);
      return;
    }
    const modelOpts = lastModels.map((m) => `<option value="${esc(m.id)}">${esc(modelShortName(m))}</option>`).join("");
    const groupOpts = lastGroups.map((g) => `<option value="${esc(g.name)}">${esc(g.name)}</option>`).join("")
      + `<option value="__new__">New group\u2026</option>`;
    addHost.innerHTML = `<div class="card"><h3>${icon("server")} Add a backend<span class="info-tip" title="The backend is created with the launch parameters below (32K context, 512 batch, and the model's catalog default offload unless changed); a tensor split is assigned from live GPU telemetry when available unless GPUs are picked under Placement. Joining an existing group makes the gateway share that group's traffic across its members; a new group gives the backend its own model name.">${icon("info")}</span></h3>
    <div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:10px">
      <label style="font-size:12px;color:var(--text2)">Name<span class="info-tip" title="lowercase, digits, hyphens">${icon("info")}</span><br>
        <input id="addBackName" type="text" placeholder="nemotron" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Model<br>
        <select id="addBackModel" style="${SELECT_STYLE};width:100%">${modelOpts}</select></label>
      <label style="font-size:12px;color:var(--text2)">Port<br>
        <input id="addBackPort" type="number" value="${nextFreePort()}" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Gateway group<br>
        <select id="addBackGroup" style="${SELECT_STYLE};width:100%">${groupOpts}</select></label>
    </div>
    <label id="addBackNewGroupWrap" style="font-size:12px;color:var(--text2);display:none;margin-top:10px">New group name<span class="info-tip" title="Defaults to the backend name.">${icon("info")}</span><br>
      <input id="addBackNewGroup" type="text" placeholder="nemotron" style="${SELECT_STYLE};width:100%"></label>
    <label style="font-size:12px;color:var(--text2);display:block;margin-top:10px" title="The gateway routes enabled backends; it can be started right away.">
      <input id="addBackEnabled" type="checkbox" checked> Enabled</label>
    <div id="addBackParams"></div>
    <div id="addBackStrip"></div>
    <div class="card-actions"><button class="btn" id="addBackSubmit">${icon("plus")} Add backend</button>
      <button class="btn icon-only" id="addBackCancel" title="Cancel" aria-label="Cancel">${icon("x")}</button></div>
    <div id="addBackMsg" style="font-size:12px;margin-top:6px"></div>
  </div>`;
    const stripEl = addHost.querySelector("#addBackStrip") as HTMLElement | null;
    const modelSelEl = addHost.querySelector("#addBackModel") as HTMLSelectElement | null;

    /* Launch parameters (tracker #49): the same fields the Edit
     * Flags and Placement forms expose for an existing backend,
     * rendered by the same helpers and collected into this draft
     * for backend_add. ctx/batch start at the creation defaults
     * and survive a model change (they are model-independent; only
     * their rated-context annotation follows the model). The
     * offload draft and the placement selection are
     * model-dependent: each backend_add_preview payload resets
     * them — the slider to the model's catalog default position,
     * the chips to the split creation would assign. */
    const pDraft: { ctx: number; batch: number; parallel: number | null } = { ctx: 32768, batch: 512, parallel: null };
    /** The add form's KV cache draft: starts at the preview's
     *  predicted-best rung for the model (the brief's starting
     *  point for a new backend), then follows the operator's
     *  changes to the cache selects; carried by backend_add. */
    let pCache: CacheSelectionLike | null = null;
    let preview: BackendAddPreview | null = null;
    let offLayers: number | null = null;
    /** A released offload position the guardrail blocks, awaiting
     *  the operator's decision in the form. `override` records the
     *  decision to create with an explicit override (backend_add
     *  journals it, as backend_set_offload does); without it the
     *  position must be snapped or moved before submit. */
    let offPending: { layers: number; override: boolean } | null = null;
    /** GPU indices the operator unchecked in the Placement chips.
     *  gpuTouched separates the previewed selection (creation
     *  auto-assigns) from an explicit one (creation writes the
     *  even split across it). */
    const gpuExcluded = new Set<number>();
    let gpuTouched = false;
    let previewSeq = 0;
    /** The predicted-throughput line for the add form's draft
     *  (design §6, shared with the Edit Flags form): the last
     *  measurement_lookup result for the configuration this
     *  form would create, debounced like the edit form's. */
    let addPredictedHtml = "";
    let addPredictedSeq = 0;
    let addPredictedTimer: number | null = null;
    const runAddLookup = async (): Promise<void> => {
      const modelId = modelSelEl?.value ?? "";
      if (!modelId) return;
      const seq = ++addPredictedSeq;
      const r = await measurementLookup({
        model_id: modelId,
        n_ctx: pDraft.ctx,
        n_batch: pDraft.batch,
        offload_layers: offLayers ?? preview?.offload?.layers ?? null,
        parallel_slots: pDraft.parallel,
        gpu_indices: gpuTouched ? selectedGpus() : null,
      });
      if (disposed || seq !== addPredictedSeq) return;
      addPredictedHtml = r.data ? predictedLineHtml(r.data.exact, r.data.near) : "";
      const slot = paramsEl?.querySelector("[data-add-predicted-slot]");
      if (slot) slot.innerHTML = addPredictedHtml;
    };
    const scheduleAddLookup = (): void => {
      if (addPredictedTimer !== null) window.clearTimeout(addPredictedTimer);
      addPredictedTimer = window.setTimeout(() => {
        addPredictedTimer = null;
        void runAddLookup();
      }, 300);
    };
    const paramsEl = addHost.querySelector("#addBackParams") as HTMLElement | null;
    const selectedGpus = (): number[] =>
      lastGpuList.map((g) => Number(g.id)).filter((ix) => !gpuExcluded.has(ix));
    /* Model summary strip (add-backend audit Q3/Q8): Size, then the
     * creation requirement as the backend card's GPU RAM / System
     * RAM tiles (backend_add_preview's `memory` — the same
     * requirement model, at the form's context/slot draft), then
     * the split preview: one segmented bar, one segment per GPU the
     * placement uses, width = its share of the split, keyed
     * "GPU n — %", the required GiB in each segment's tooltip. For
     * an offload-capable model the two weight tiles follow the
     * offload draft through the same per-position pricing the
     * slider readout uses. A figure the payload does not carry is
     * omitted, never zero-filled; no placement = no bar. */
    const splitColors = ["#4a9eff", "#a371f7", "#39c5cf", "#d29922", "#3fb950", "#f85149"];
    const paintStrip = (): void => {
      if (!stripEl || !modelSelEl) return;
      const m = lastModels.find((x) => x.id === modelSelEl.value);
      if (!m) { stripEl.innerHTML = ""; return; }
      const size = m.size_bytes ? fmtBytes(m.size_bytes)
        : m.catalog_size_bytes !== null ? `~${fmtBytes(m.catalog_size_bytes)} (estimate)` : null;
      const mem = preview && preview.model_id === m.id ? preview.memory ?? null : null;
      const tiles: [string, string, string][] = [];
      if (size !== null) tiles.push(["Size", size, ""]);
      if (mem) {
        let gpuMib = mem.gpu_ram_mib;
        let sysMib = mem.sys_ram_mib;
        if (preview?.offload) {
          const figs = offloadFigures(preview.offload, offLayers ?? preview.offload.layers);
          gpuMib = figs.gpuMib;
          sysMib = figs.sysMib;
        }
        tiles.push(["GPU RAM", `${fmtGb(gpuMib / 1024)} GiB`, "GPU-resident weights of the backend this form would create"]);
        tiles.push(["System RAM", `${fmtGb(sysMib / 1024)} GiB`, "Weights held in system RAM (routed experts under expert offload)"]);
      }
      let barHtml = "";
      const split = preview && preview.model_id === m.id ? preview.tensor_split : [];
      const entries: { gpu: number; share: number; required: number | null; total: number | null }[] = mem && mem.per_gpu.length
        ? mem.per_gpu.map((p) => ({ gpu: p.gpu, share: p.share, required: p.required_mib, total: p.total_mib }))
        : lastGpuList
          .map((g, p) => ({ gpu: Number(g.id), share: split[p] ?? 0, required: null, total: null }))
          .filter((e) => e.share > 0);
      if (entries.length) {
        const nameOf = (ix: number): string => lastGpuList.find((g) => Number(g.id) === ix)?.name ?? "";
        const segs = entries.map((e) => {
          const nm = nameOf(e.gpu);
          const req = e.required !== null && e.total !== null ? ` · Required: ${fmtGb(e.required / 1024)} of ${fmtGb(e.total / 1024)} GiB` : "";
          return { pct: e.share * 100, color: splitColors[e.gpu % splitColors.length], label: `GPU ${e.gpu}${nm ? ` · ${nm}` : ""} — ${trim1(e.share * 100)}%${req}` };
        });
        const keys = entries.map((e, i) => `<span title="${esc(segs[i].label)}"><span class="sw" style="background:${segs[i].color}"></span>GPU ${e.gpu} — ${trim1(e.share * 100)}%</span>`).join("");
        barHtml = `<div data-add-split-bar style="margin-top:10px">
        <span style="font-size:12px;color:var(--text2)">Split</span><span class="info-tip" title="The tensor split creation would assign: each segment is one GPU's share of the model's weights. The selection comes from live telemetry unless GPUs are picked under Placement.">${icon("info")}</span>
        <div style="margin-top:4px">${stackedBar(segs, 600, 10)}</div>
        <div class="legend" style="margin-top:4px">${keys}</div></div>`;
      }
      stripEl.innerHTML = `<div style="display:grid;grid-template-columns:repeat(4,1fr);gap:10px;margin-top:10px">
        ${tiles.map(([k, v, tip]) => `<div class="kv"${tip ? ` title="${esc(tip)}"` : ""}><span class="k">${k}</span><span class="v" style="font-size:12px">${esc(v)}</span></div>`).join("")}
      </div>${barHtml}`;
    };
    const paintParams = (): void => {
      paintStrip();
      if (!paramsEl) return;
      const m = lastModels.find((x) => x.id === modelSelEl?.value);
      const ctxMax = m?.ctx_max ? m.ctx_max : null;
      // The offload slider, when the previewed model is
      // offload-capable. The decision row carries the edit form's
      // two resolving actions with creation wording: nothing can
      // be saved yet, so an override is recorded in the draft and
      // carried by backend_add; snapping moves the draft to the
      // nearest verified position.
      let offloadHtml = "";
      if (preview?.offload) {
        const o = preview.offload;
        const n = offLayers ?? o.layers;
        let decisionHtml = "";
        if (offPending && o.guardrail && offloadIsUntested(o, offPending.layers)) {
          decisionHtml = offPending.override
            ? `<div data-add-offload-decision style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:8px">
      <span data-add-decision-text style="font-size:12px;color:#e3b341">${offPending.layers} of ${o.moe_layers} layers is unverified on this GPU \u2014 the backend will be created with an override recorded</span>
      <button class="btn" data-add-decision="snap">${icon("check")} Snap to ${o.guardrail.nearest_verified_layers} of ${o.moe_layers} layers</button>
    </div>`
            : `<div data-add-offload-decision style="display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-top:8px">
      <span data-add-decision-text style="font-size:12px;color:#e3b341">${offPending.layers} of ${o.moe_layers} layers is unverified on this GPU</span>
      <button class="btn" data-add-decision="override">${icon("warn")} Create with override</button>
      <button class="btn" data-add-decision="snap">${icon("check")} Snap to ${o.guardrail.nearest_verified_layers} of ${o.moe_layers} layers</button>
    </div>`;
        }
        offloadHtml = offloadSliderHtml({ offload: o, layers: n, ownerId: "new backend", disabled: false, decisionHtml });
      }
      // Placement chips: the Placement form's include/exclude
      // chips over the telemetry GPUs, defaulting to the split
      // creation would assign.
      let placeHtml = "";
      // The resulting split renders once, as the strip's split bar
      // directly below (its keys carry the percentages).
      if (lastGpuList.length) {
        placeHtml = `<div style="margin-top:12px">
        <span style="font-size:12px;color:var(--text2)">Placement (tensor split)</span><span class="info-tip" title="The split divides the model's weights across the selected GPUs, in nvidia-smi index order. The selection starts at the split a creation would assign from live telemetry; changing it makes the split an even division across the selected GPUs instead.">${icon("info")}</span>
        <div class="gpu-chip-row">${lastGpuList.map((g) => {
          const ix = Number(g.id);
          const on = !gpuExcluded.has(ix);
          return `<label class="gpu-chip${on ? " on" : ""}" title="${esc(g.name)}"><span class="gpu-chip-top"><input type="checkbox" data-add-place-gpu="${ix}"${on ? " checked" : ""}> GPU ${ix}</span><span class="gpu-chip-name">${esc(gpuShortName(g.name))}</span></label>`;
        }).join("")}</div>
      </div>`;
      } else {
        placeHtml = `<div style="font-size:12px;color:var(--text2);margin-top:12px">Placement: auto<span class="info-tip" title="No GPU telemetry yet \u2014 a tensor split cannot be previewed or selected; creation leaves the split unset, as it does today.">${icon("info")}</span></div>`;
      }
      // The cache controls for the backend being created: the
      // draft starts at the preview's predicted-best rung; the
      // footprint line re-prices from the ladder rung matching
      // the draft's K/V types when the draft differs from the
      // preview's own selection.
      let cacheHtml = "";
      if (preview?.cache) {
        const sel: CacheSelectionLike = pCache ?? (preview.cache.best ? { k: preview.cache.best.k, v: preview.cache.best.v, flash_attn: preview.cache.best.flash_attn } : preview.cache.selection);
        let renderPreview: CachePreview = preview.cache;
        if (!sameCacheSelection(sel, preview.cache.selection)) {
          const rung = preview.cache.ladder.find((r) => r.k === sel.k && r.v === sel.v);
          if (rung) renderPreview = { ...preview.cache, kv_mib: rung.kv_mib, total_mib: rung.total_mib, fits_current: rung.fits, selection: sel };
        }
        cacheHtml = flagsCacheHtml({ sel, savedCustom: false, savedPresent: false, preview: renderPreview, fieldAttr: "data-add-cache-field", bestAttr: "data-add-cache-best", disabled: false });
      }
      paramsEl.innerHTML = `<div style="margin-top:12px;border-top:1px solid var(--border);padding-top:10px">
      <div style="font-size:12px;font-weight:600">${icon("gear")} Launch parameters</div>
      ${flagsCtxBatchHtml({ ctx: pDraft.ctx, batch: pDraft.batch, ctxMax, fieldAttr: "data-add-flags-field", disabled: false })}
      ${flagsParallelHtml({ value: pDraft.parallel, savedCustom: false, fieldAttr: "data-add-flags-field", disabled: false })}
      ${cacheHtml}
      ${offloadHtml}
      ${flagsRatedLineHtml({ ctxMax, modelId: m?.id ?? null, savedText: "Saved when the backend is added; applies from its first start. Placement is set by the GPU selection below." })}
      <div data-add-predicted-slot>${addPredictedHtml}</div>
      ${placeHtml}
    </div>`;
    };
    const resetPlacementFromPreview = (): void => {
      gpuExcluded.clear();
      gpuTouched = false;
      const split = preview?.tensor_split ?? [];
      lastGpuList.forEach((g, p) => {
        // No assigned split (single GPU, or none yet): an unsplit
        // creation loads on the first GPU — the Placement form's
        // fallback selection for the same state.
        const on = split.length ? (split[p] ?? 0) > 0 : p === 0;
        if (!on) gpuExcluded.add(Number(g.id));
      });
    };
    const loadPreview = async (modelChanged: boolean): Promise<void> => {
      const modelId = modelSelEl?.value ?? "";
      if (!modelId) return;
      const seq = ++previewSeq;
      const r = await backendAddPreview(modelId, gpuTouched ? selectedGpus() : undefined, pDraft.ctx, pDraft.parallel ?? undefined);
      if (disposed || seq !== previewSeq) return;
      if (r.error || !r.data) { preview = null; paintParams(); scheduleAddLookup(); return; }
      preview = r.data;
      if (modelChanged) {
        offLayers = preview.offload ? preview.offload.layers : null;
        offPending = null;
        pCache = preview.cache ? (preview.cache.best ? { k: preview.cache.best.k, v: preview.cache.best.v, flash_attn: preview.cache.best.flash_attn } : { ...preview.cache.selection }) : null;
        resetPlacementFromPreview();
      }
      paintParams();
      scheduleAddLookup();
    };
    modelSelEl?.addEventListener("change", () => { paintStrip(); void loadPreview(true); });
    paramsEl?.addEventListener("input", (ev) => {
      const t = ev.target as HTMLElement;
      if (t.hasAttribute("data-offload-slider")) {
        if (!preview?.offload) return;
        const n = Number((t as HTMLInputElement).value);
        offLayers = n;
        scheduleAddLookup();
        const card = t.closest(".card");
        if (card) paintOffloadReadout(card, preview.offload, n);
        // The strip's GPU RAM / System RAM tiles follow the draft.
        paintStrip();
        // Keep a pending decision in step with the draft (the
        // edit form's rule): its text follows a still-blocked
        // position, and the row hides while the draft sits on a
        // verified position; the release handler re-decides.
        if (offPending) {
          const dec = paramsEl.querySelector("[data-add-offload-decision]") as HTMLElement | null;
          if (dec) {
            if (offloadIsUntested(preview.offload, n)) {
              offPending.layers = n;
              dec.style.display = "";
              const dt = dec.querySelector("[data-add-decision-text]");
              if (dt) dt.textContent = `${n} of ${preview.offload.moe_layers} layers is unverified on this GPU${offPending.override ? " \u2014 the backend will be created with an override recorded" : ""}`;
            } else {
              dec.style.display = "none";
            }
          }
        }
        return;
      }
      const field = t.getAttribute("data-add-flags-field");
      if (field) { applyFlagsField(pDraft, field, (t as HTMLInputElement).value, false); scheduleAddLookup(); }
    });
    paramsEl?.addEventListener("change", (ev) => {
      const t = ev.target as HTMLElement;
      if (t.hasAttribute("data-offload-slider")) {
        if (!preview?.offload) return;
        const n = Number((t as HTMLInputElement).value);
        offLayers = n;
        // Release decides the pending state: a guardrail-blocked
        // position becomes a decision the operator must make (an
        // override confirmed for one position does not carry to
        // another); a verified position clears it.
        if (offloadIsUntested(preview.offload, n)) {
          if (!offPending || offPending.layers !== n) offPending = { layers: n, override: false };
        } else {
          offPending = null;
        }
        paintParams();
        scheduleAddLookup();
        return;
      }
      if (t.hasAttribute("data-add-place-gpu")) {
        const ix = Number(t.getAttribute("data-add-place-gpu"));
        if ((t as HTMLInputElement).checked) gpuExcluded.delete(ix);
        else gpuExcluded.add(ix);
        gpuTouched = true;
        // The placement drives the guardrail evaluation: refresh
        // the preview against the explicit selection.
        void loadPreview(false);
        return;
      }
      if (t.hasAttribute("data-add-cache-field")) {
        const read = (f: string): string => (paramsEl.querySelector(`[data-add-cache-field="${f}"]`) as HTMLSelectElement | null)?.value ?? "";
        const sel: CacheSelectionLike = { k: read("cache-k") as CacheTypeName, v: read("cache-v") as CacheTypeName, flash_attn: read("flash-attn") as FlashAttnName };
        if (CACHE_TYPES.includes(sel.k) && CACHE_TYPES.includes(sel.v) && FLASH_ATTN_MODES.includes(sel.flash_attn)) { pCache = sel; paintParams(); scheduleAddLookup(); }
        return;
      }
      const field = t.getAttribute("data-add-flags-field");
      if (field && applyFlagsField(pDraft, field, (t as HTMLInputElement).value, true)) { paintParams(); scheduleAddLookup(); void loadPreview(false); }
    });
    paramsEl?.addEventListener("click", (ev) => {
      const bestBtn = (ev.target as HTMLElement).closest("[data-add-cache-best]") as HTMLElement | null;
      if (bestBtn && preview?.cache?.best) {
        const b = preview.cache.best;
        pCache = { k: b.k, v: b.v, flash_attn: b.flash_attn };
        paintParams();
        scheduleAddLookup();
        return;
      }
      const t = (ev.target as HTMLElement).closest("[data-add-decision]") as HTMLElement | null;
      if (!t || !preview?.offload || !offPending) return;
      if (t.getAttribute("data-add-decision") === "override") {
        offPending.override = true;
      } else {
        // Snap: move the draft to the nearest verified position.
        offLayers = preview.offload.guardrail?.nearest_verified_layers ?? offPending.layers;
        offPending = null;
      }
      paintParams();
    });
    paintParams();
    void loadPreview(true);
    const groupSelEl = addHost.querySelector("#addBackGroup") as HTMLSelectElement | null;
    const newWrap = addHost.querySelector("#addBackNewGroupWrap") as HTMLElement | null;
    groupSelEl?.addEventListener("change", () => {
      if (newWrap) newWrap.style.display = groupSelEl.value === "__new__" ? "block" : "none";
    });
    addHost.querySelector("#addBackCancel")?.addEventListener("click", closeAddForm);
    const submit = addHost.querySelector("#addBackSubmit") as HTMLButtonElement | null;
    submit?.addEventListener("click", async () => {
      const val = (sel: string): string => (addHost.querySelector(sel) as HTMLInputElement | null)?.value.trim() ?? "";
      const name = val("#addBackName").toLowerCase();
      const modelId = (addHost.querySelector("#addBackModel") as HTMLSelectElement | null)?.value ?? "";
      const port = Number(val("#addBackPort"));
      const groupSel = (addHost.querySelector("#addBackGroup") as HTMLSelectElement | null)?.value ?? "";
      const createGroup = groupSel === "__new__";
      const group = createGroup ? (val("#addBackNewGroup").toLowerCase() || name) : groupSel;
      const enabled = (addHost.querySelector("#addBackEnabled") as HTMLInputElement | null)?.checked ?? true;
      const msg = addHost.querySelector("#addBackMsg") as HTMLElement | null;
      if (!name || !modelId || !group || !Number.isInteger(port) || port <= 0) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Name, model, port, and group are required.</span>`;
        return;
      }
      // Launch parameters: the same validation the create op
      // applies (context range; batch within 1..=context), checked
      // here so the error reads in the form's own terms; the op
      // re-validates regardless.
      if (!Number.isInteger(pDraft.ctx) || pDraft.ctx < 1024 || pDraft.ctx > 1048576) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Context length must be between 1,024 and 1,048,576.</span>`;
        return;
      }
      if (!Number.isInteger(pDraft.batch) || pDraft.batch < 1 || pDraft.batch > pDraft.ctx) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Batch size must be between 1 and the context length.</span>`;
        return;
      }
      // Parallel slots: the same bounds the create op applies
      // (backend_set_parallel's range), checked here in the form's
      // terms; an untouched field (null) sends nothing and the
      // backend is created with no --parallel token.
      if (pDraft.parallel !== null && (!Number.isInteger(pDraft.parallel) || pDraft.parallel < PARALLEL_MIN || pDraft.parallel > PARALLEL_MAX)) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Parallel slots must be between ${PARALLEL_MIN} and ${fmtCount(PARALLEL_MAX)}.</span>`;
        return;
      }
      // KV cache: the pairing llama.cpp refuses cannot be carried
      // into creation (the op refuses it too).
      if (pCache && cacheVQuantizedFaOff(pCache)) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">A quantized V cache requires flash attention: choose flash attention auto or on.</span>`;
        return;
      }
      // An undecided guardrail-blocked offload position cannot be
      // carried into creation (the op would refuse it without the
      // override): the operator decides here first.
      if (offPending && !offPending.override) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Offload position ${offPending.layers} of ${preview?.offload?.moe_layers ?? ""} layers is unverified on this GPU \u2014 create with an override or snap to a verified position.</span>`;
        return;
      }
      if (gpuTouched && !selectedGpus().length) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Select at least one GPU for the split.</span>`;
        return;
      }
      submit.disabled = true;
      const r = await addBackend({
        id: name, modelId, port, group, createGroup, enabled,
        nCtx: pDraft.ctx,
        nBatch: pDraft.batch,
        offloadLayers: preview?.offload ? (offLayers ?? preview.offload.layers) : undefined,
        offloadOverride: offPending?.override ? true : undefined,
        gpuIndices: gpuTouched ? selectedGpus() : undefined,
        parallel: pDraft.parallel ?? undefined,
        cacheK: pCache?.k,
        cacheV: pCache?.v,
        flashAttn: pCache?.flash_attn,
      });
      if (disposed) return;
      if (r.error) {
        if (msg) msg.innerHTML = `<span style="color:var(--err)">Add failed: ${esc(r.error)}</span>`;
        submit.disabled = false;
        return;
      }
      const modelRow = lastModels.find((x) => x.id === modelId);
      addHost.innerHTML = `<div class="card"><h3>${icon("check")} Backend added</h3>
        <div style="display:grid;gap:4px;margin-top:8px;font-size:12px">
          <div><span style="color:var(--text2)">Backend:</span> ${esc(name)}</div>
          <div><span style="color:var(--text2)">Model:</span> ${esc(modelRow ? modelShortName(modelRow) : modelId)}</div>
          <div><span style="color:var(--text2)">Port:</span> ${port}</div>
          <div><span style="color:var(--text2)">Group:</span> ${esc(group)}</div>
          <div><span style="color:var(--text2)">Enabled:</span> ${enabled ? "yes" : "no"}</div>
          <div><span style="color:var(--text2)">Context:</span> ${fmtCount(pDraft.ctx)}</div>
          <div><span style="color:var(--text2)">Batch:</span> ${fmtCount(pDraft.batch)}</div>
          ${pDraft.parallel !== null ? `<div><span style="color:var(--text2)">Parallel slots:</span> ${pDraft.parallel}</div>` : ""}
          ${pCache ? `<div><span style="color:var(--text2)">KV cache:</span> ${esc(cacheTokenText(pCache))}</div>` : ""}
          ${preview?.offload ? `<div><span style="color:var(--text2)">Offload:</span> ${esc(offloadTokenText(preview.offload, offLayers ?? preview.offload.layers))}${offPending?.override ? " (override recorded)" : ""}</div>` : ""}
          ${gpuTouched ? `<div><span style="color:var(--text2)">GPUs:</span> ${esc(selectedGpus().map((ix) => `GPU ${ix}`).join(", "))}</div>` : ""}
          <div><span style="color:var(--text2)">Activation plan:</span> recorded<span class="info-tip" title="The change is recorded in the activation plan at the top of this page — the gateway routes the new backend after the plan is executed.">${icon("info")}</span></div>
        </div>
        <div class="card-actions"><button class="btn icon-only" id="addBackDone" title="Done" aria-label="Done">${icon("check")}</button></div></div>`;
      addHost.querySelector("#addBackDone")?.addEventListener("click", closeAddForm);
      await load();
    });
  };
  const addBtn = root.querySelector("#backAdd");
  addBtn?.addEventListener("click", openAddForm);

  const onClick = async (ev: Event): Promise<void> => {
    const target = ev.target as HTMLElement;
    const menuToggle = target.closest("[data-back-menu]") as HTMLElement | null;
    if (menuToggle) {
      const menuId = menuToggle.getAttribute("data-back-menu") ?? "";
      const wasOpen = openMenus.has(menuId);
      openMenus.clear();
      if (!wasOpen && menuId) openMenus.add(menuId);
      paint();
      return;
    }
    const menuItem = target.closest("[data-back-menu-item]") as HTMLElement | null;
    if (menuItem) openMenus.delete(menuItem.getAttribute("data-back-id") ?? "");
    const hadOpenMenu = openMenus.size > 0;
    if (!menuItem && !target.closest("[data-back-menu-wrap]") && hadOpenMenu) {
      openMenus.clear();
    }
    const pt = target.closest("[data-plan-act]") as HTMLElement | null;
    if (pt) {
      if (hadOpenMenu) paint();
      await handlePlanAction(pt.getAttribute("data-plan-act") ?? "", planUi, paintPlan, load, lastPlan);
      return;
    }
    const t = target.closest("[data-back-act]") as HTMLElement | null;
    if (!t) {
      if (hadOpenMenu && !target.closest("[data-back-menu-wrap]")) paint();
      return;
    }
    const id = t.getAttribute("data-back-id") ?? "";
    const act = t.getAttribute("data-back-act") ?? "";

    if (act === "tuning") {
      // Per-backend tuning (Addendum B §B.2): the sweep lives
      // on the Optimize page; preselect this backend there.
      try { sessionStorage.setItem("tuningBackend", id); } catch { /* storage unavailable */ }
      location.hash = "#/optimize";
      return;
    }

    if (act === "details") {
      const opening = !openDetails.has(id);
      if (opening) openDetails.add(id); else openDetails.delete(id);
      paint();
      // The expansion's latest-lines block wants this backend's
      // tail now, not at the next poll's load.
      if (opening) void fetchLogTails();
      return;
    }

    if (act === "predict-refresh") {
      // A Measure run against the running backend, awaited here
      // the way the Optimize view awaits it: the tile carries
      // the Measuring state from the wire-level map while the
      // invoke is outstanding, the field updates from the
      // refreshed rows on completion (no page reload), and a
      // failure lands on the card's note — never silent. The
      // op's own refusals (job slot busy, backend stopped
      // mid-run) arrive as the error and surface the same way.
      if (predictRefreshing.has(id)) return;
      const row = lastRows.find((r) => r.id === id);
      if (!row || !row.configured || row.state !== "running") return;
      predictRefreshing.set(id, Date.now());
      msgs.delete(id);
      paint();
      startPredictTicker();
      const r = await optimizeMeasureBackend(id);
      predictRefreshing.delete(id);
      if (!predictRefreshing.size) stopPredictTicker();
      if (disposed) return;
      if (r.error) {
        msgs.set(id, {
          text: r.error === "measurement cancelled"
            ? "Prediction refresh cancelled."
            : `Prediction refresh failed — ${r.error}`,
          err: true,
        });
        paint();
      } else {
        msgs.delete(id);
        await load();
      }
      return;
    }

    if (act === "flags") {
      // The one Edit entry point: it opens the whole Edit
      // session — the Edit panel (model, flags) and the
      // Placement section below it — or closes the session when
      // any part of it is open.
      const row = lastRows.find((r) => r.id === id);
      if (!row) return;
      if (flagsFor === id || placeFor === id) closeEditSession();
      else { openFlagsForm(row); openPlaceForm(row); }
      return;
    }

    if (act === "flags-cancel") {
      closeFlagsForm();
      return;
    }

    if (act === "place-cancel") {
      closePlaceForm();
      return;
    }

    if (act === "place-propose") {
      if (placeBusy) return;
      const row = lastRows.find((r) => r.id === id);
      const model = lastModels.find((m) => row !== undefined && sameModelFile(row.model_file, m.gguf_path));
      if (!row || !model) return;
      if (!lastGpuList.length) {
        msgs.set(id, { text: "No GPU telemetry yet \u2014 a split cannot be proposed.", err: true });
        paint();
        return;
      }
      // Compose the split: preserve the backend's configured split
      // when it covers exactly the selected set of GPUs (the norm
      // right after loading); otherwise an even split across the
      // selection. Positions follow the telemetry GPU order, which
      // is nvidia-smi index order — the same order tensor_split
      // uses.
      const usedPos = lastGpuList
        .map((g, p) => (!placeExcluded.has(Number(g.id)) ? p : -1))
        .filter((p) => p >= 0);
      if (!usedPos.length) {
        msgs.set(id, { text: "Select at least one GPU for the split.", err: true });
        paint();
        return;
      }
      const cfgSplit = row.tensor_split ?? [];
      const nz = cfgSplit.map((f, i) => (f > 0 ? i : -1)).filter((i) => i >= 0);
      const sameSet = cfgSplit.length === lastGpuList.length
        && nz.length === usedPos.length
        && nz.every((i) => usedPos.includes(i));
      const split = sameSet
        ? [...cfgSplit]
        : lastGpuList.map((_, p) => (usedPos.includes(p) ? 1 / usedPos.length : 0));
      placeBusy = true;
      paint();
      try {
        const r = await placementPropose(model.id, split, id);
        if (disposed) return;
        if (r.error || !r.data) {
          msgs.set(id, { text: `Propose failed: ${r.error ?? "the command returned no projection"}`, err: true });
          placeProjection = null;
        } else {
          placeProjection = { backendId: id, modelId: model.id, data: r.data };
        }
      } finally {
        placeBusy = false;
        if (!disposed) paint();
      }
      return;
    }

    if (act === "place-apply") {
      if (placeBusy || !placeProjection || placeProjection.backendId !== id) return;
      const proj = placeProjection;
      // The projection prices the model it was proposed for. If the
      // backend's model changed since (a model assignment reloads
      // the rows), the projection is stale: drop it rather than
      // applying the old model's split to the new model
      // (code-review 5.7).
      const prow = lastRows.find((r) => r.id === id);
      const currentModel = lastModels.find((m) => prow !== undefined && sameModelFile(prow.model_file, m.gguf_path));
      if (!prow || !currentModel || currentModel.id !== proj.modelId) {
        msgs.set(id, { text: "The backend's model changed since this split was proposed — propose again to price the current model.", err: true });
        placeProjection = null;
        paint();
        return;
      }
      placeBusy = true;
      paint();
      try {
        const r = await placementApply(proj.modelId, proj.data.split, id);
        if (disposed) return;
        if (r.error) {
          // Keep the form and projection open with the error on the
          // card — closing would read as if the apply had happened.
          msgs.set(id, { text: `Split not saved: ${r.error}`, err: true });
          paint();
        } else if (r.data.persisted === false) {
          // Applied in memory only (code-review 5.2): say so on
          // the card instead of the saved message — the split is
          // lost when the app restarts.
          msgs.set(id, { text: "Split applied for this session only — it was not saved and will be lost when the app restarts.", err: true });
          placeFor = null;
          placeProjection = null;
          placeExcluded.clear();
          await load();
        } else {
          msgs.set(id, { text: `Split saved to ${id}. Restart the backend to use it.`, err: false });
          placeFor = null;
          placeProjection = null;
          placeExcluded.clear();
          await load();
        }
      } finally {
        placeBusy = false;
      }
      return;
    }

    if (act === "flags-save") {
      // The Edit panel's Save: the model draft applies first —
      // a failed assignment saves nothing else and keeps the
      // panel open — then the ctx/batch draft goes through the
      // flags save. The card note names both halves of the
      // outcome, so a flags failure after a landed model change
      // never reads as a full save or a full failure.
      inFlight.add(id);
      paint();
      try {
        const mres = await saveEditModel(id);
        if (disposed) return;
        if (mres === "failed") { paint(); return; }
        const modelNote = mres === "saved" ? `Model set to ${flagsEditModelId}. ` : "";
        const r = await updateBackendFlags(id, flagsCtx, flagsBatch);
        if (disposed) return;
        if (r.error) {
          // Keep the form open with the error on the card — closing on
          // a rejected value would read as if the save had happened.
          msgs.set(id, { text: mres === "saved" ? `Model set to ${flagsEditModelId} — flags not saved: ${r.error}` : `Flags not saved: ${r.error}`, err: true });
          paint();
        } else if (r.data.persisted === false) {
          // Code-review 5.2: a non-persisted save is not a save.
          msgs.set(id, { text: `${modelNote}Flags applied for this session only — they were not saved and will be lost when the app restarts.`, err: true });
          closeEditSession();
          await load();
        } else {
          msgs.set(id, { text: `${modelNote}Flags saved (context ${fmtCount(flagsCtx)}, batch ${fmtCount(flagsBatch)}) — applies on next start.`, err: false });
          closeEditSession();
          await load();
        }
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "flags-mode-advanced") {
      // Enter advanced flags mode (tracker #60). The flip writes
      // only the mode — the saved flags do not change — but it
      // closes the structured form, so unsaved structured edits
      // (a moved ctx/batch draft or a pending offload decision)
      // are discarded: confirm first, per the app's confirm
      // idiom, only when there is something to lose.
      const row = lastRows.find((r) => r.id === id);
      if (!row) return;
      const dirty = flagsCtx !== (row.n_ctx ?? 32768)
        || flagsBatch !== (row.n_batch ?? 512)
        || offloadPending !== null;
      if (dirty && !confirm("Switch to advanced flags? Unsaved changes in this form will be discarded. The saved flags themselves do not change.")) return;
      inFlight.add(id);
      paint();
      try {
        const r = await setBackendFlagsMode(id, true);
        if (disposed) return;
        if (r.error) {
          msgs.set(id, { text: `Advanced mode not enabled: ${r.error}`, err: true });
          paint();
          return;
        }
        msgs.set(id, { text: "Advanced flags mode on — the flags are edited as text in the panel.", err: false });
        await load();
        // Reopen the panel in its advanced form, on the saved
        // string the mode flip returned (byte-identical to what
        // was live — entering the mode writes no flags).
        const fresh = lastRows.find((x) => x.id === id);
        if (fresh) { flagsFor = null; openFlagsForm(fresh); }
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "flags-mode-structured") {
      // Leave advanced mode (tracker #60): the structured form
      // decodes the SAVED raw string through the existing
      // decoders, so unsaved text edits would be lost — confirm
      // when the text differs from the saved string.
      const row = lastRows.find((r) => r.id === id);
      if (!row) return;
      if (flagsRawText !== (row.raw_flags ?? "")
        && !confirm("Return to the structured form? The unsaved flag edits in the text box will be discarded. The saved flags do not change.")) return;
      inFlight.add(id);
      paint();
      try {
        const r = await setBackendFlagsMode(id, false);
        if (disposed) return;
        if (r.error) {
          msgs.set(id, { text: `Structured form not restored: ${r.error}`, err: true });
          paint();
          return;
        }
        msgs.set(id, { text: "Structured flags form restored — it shows the values decoded from the saved flags.", err: false });
        await load();
        const fresh = lastRows.find((x) => x.id === id);
        if (fresh) { flagsFor = null; openFlagsForm(fresh); }
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "flags-raw-save") {
      // Save the advanced editor's text (tracker #60). The op
      // journals like the other flags writes and applies on the
      // next start; nothing restarts. Warnings ride the result
      // and never block (the operator saw the same list live
      // while typing; the server's list replaces the draft's in
      // wire state). On success the Edit surface closes like the
      // structured Save's — see closeEditSession.
      inFlight.add(id);
      paint();
      try {
        // The Edit panel's model draft rides this Save too, with
        // the structured Save's rule: a failed assignment saves
        // nothing else and keeps the panel open.
        const mres = await saveEditModel(id);
        if (disposed) return;
        if (mres === "failed") { paint(); return; }
        const modelNote = mres === "saved" ? `Model set to ${flagsEditModelId}. ` : "";
        const text = flagsRawText;
        const r = await setBackendRawFlags(id, text.trim() === "" ? null : text);
        if (disposed) return;
        if (r.error) {
          msgs.set(id, { text: mres === "saved" ? `Model set to ${flagsEditModelId} — flags not saved: ${r.error}` : `Flags not saved: ${r.error}`, err: true });
          paint();
          return;
        }
        flagsRawText = r.data.raw_flags ?? "";
        flagsRawWarnings = r.data.warnings ?? [];
        msgs.set(id, {
          text: modelNote + (r.data.changed
            ? "Raw flags saved — applies on next start."
            : "Raw flags unchanged."),
          err: false,
        });
        // A successful save closes the Edit surface (Pete's
        // report, 2026-10-20) — the advanced editor previously
        // stayed open on the adopted text by design, which is
        // the behavior Pete filed against. The adopted text and
        // warnings above keep the wire state coherent; reopening
        // Edit reseeds from the saved row regardless.
        closeEditSession();
        await load();
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "offload-override") {
      // Save the pending guardrail-blocked position with an explicit
      // override. A server error surfaces on the card and the
      // decision row stays (the helper keeps the pending state and
      // the draft), so the operator can still snap instead.
      const pending = offloadPending?.backendId === id ? offloadPending : null;
      if (!pending) return;
      await saveOffloadPosition(id, pending.layers, true);
      return;
    }

    if (act === "offload-snap") {
      // Move the draft to the nearest verified position and save it
      // without an override. On error the draft returns to the
      // blocked position, keeping the decision row coherent.
      const pending = offloadPending?.backendId === id ? offloadPending : null;
      const row = lastRows.find((r) => r.id === id);
      const target = row?.offload?.guardrail?.nearest_verified_layers;
      if (!pending || !row?.offload || target === undefined) return;
      flagsOffloadLayers = target;
      const ok = await saveOffloadPosition(id, target, false);
      if (!ok && !disposed) {
        flagsOffloadLayers = pending.layers;
        paint();
      }
      return;
    }

    if (act === "offload-set-verified") {
      // The blocked card line's resolving action: save the nearest
      // verified position (no override). On error the helper has
      // already surfaced the message on the card.
      const row = lastRows.find((r) => r.id === id);
      const target = row?.offload?.guardrail?.nearest_verified_layers;
      if (!row?.offload || target === undefined) return;
      await saveOffloadPosition(id, target, false);
      return;
    }

    if (act === "offload-override-arm") {
      // Arm the deliberate override's confirmation: the button
      // re-renders naming the exact position (layers + placed CC
      // set) the confirm will record. No command fires here.
      confirmOverride.add(id);
      paint();
      return;
    }

    if (act === "offload-override-confirm") {
      // Record the deliberate override for the SAVED position.
      // The key comes from the live row (saved layers + the
      // guardrail's placed CC set); the server re-derives it and
      // refuses any mismatch, so a placement or position change
      // since the arm surfaces as an error, never a wider grant.
      const row = lastRows.find((r) => r.id === id);
      const g = row?.offload?.guardrail;
      if (!row?.offload || !g) { confirmOverride.delete(id); paint(); return; }
      confirmOverride.delete(id);
      inFlight.add(id);
      paint();
      try {
        const r = await setBackendOffloadOverride(id, row.offload.layers, g.placed_ccs ?? [g.compute_capability]);
        if (disposed) return;
        if (r.error) msgs.set(id, { text: `Override not recorded: ${r.error}`, err: true });
        else msgs.set(id, { text: `Offload override recorded (${row.offload.layers} of ${row.offload.moe_layers} layers) — starts proceed.`, err: false });
        await load();
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "offload-override-clear") {
      // Drop the standing override: the saved position is blocked
      // again at the next start's preflight.
      inFlight.add(id);
      paint();
      try {
        const r = await clearBackendOffloadOverride(id);
        if (disposed) return;
        if (r.error) msgs.set(id, { text: `Override not cleared: ${r.error}`, err: true });
        else msgs.set(id, { text: "Offload override cleared — the saved position is blocked again.", err: false });
        await load();
      } finally {
        inFlight.delete(id);
        if (!disposed) paint();
      }
      return;
    }

    if (act === "argv") {
      if (openArgv.has(id)) { openArgv.delete(id); paint(); return; }
      if (!argvCache.has(id)) {
        // Only successful fetches are cached (per mount); a failure is
        // shown in the message area and the next toggle retries.
        inFlight.add(id);
        paint();
        try {
          const r = await getBackendLaunchCommand(id);
          if (disposed) return;
          if (r.error) msgs.set(id, { text: `Launch command failed: ${r.error}`, err: true });
          else argvCache.set(id, r.data.argv);
        } finally {
          inFlight.delete(id);
        }
        if (!argvCache.has(id)) { paint(); return; } // fetch failed: show the note, stay closed
      }
      openArgv.add(id);
      paint();
      return;
    }

    if (act === "start" || act === "restart" || act === "stop") {
      // Lifecycle actions run through the shared runner: the store's
      // pending overlay shows the transition on this card and on every
      // other surface at once, polling continues meanwhile (the server
      // reports the transition with its origin), and the result lands
      // in the card note.
      const row = lastRows.find((r) => r.id === id);
      if (act !== "stop" && row?.fit?.state === "wddm_paged" && !armedWddmStart.has(id)) {
        // WDDM paged fits start only through the two-click confirm:
        // the first click arms and states the paged amount; the
        // second click carries the confirmation to the gate.
        armedWddmStart.add(id);
        msgs.set(id, { text: `${fmtGb(row.fit.paged_mib / 1024)} GiB will page to system RAM. Click ${act === "start" ? "Start" : "Restart"} again to confirm.`, err: false });
        paint();
        return;
      }
      const allow = (act !== "stop" && armedWddmStart.has(id)) || undefined;
      armedWddmStart.delete(id);
      msgs.delete(id);
      const r = await runLifecycle({ kind: "backend", id }, act, { origin: "ui", allowWddmOvercommit: allow });
      if (disposed) return;
      if (!r.ok) msgs.set(id, { text: r.message, err: true });
      else msgs.delete(id);
      await load();
      return;
    }

    inFlight.add(id);
    paint();
    try {
      if (act === "enable" || act === "disable") {
        const r = await applyBackendConfig(id, { enabled: act === "enable" }, "OnNextRestart");
        if (r.error) msgs.set(id, { text: `${act} failed: ${r.error}`, err: true });
        else if (r.data.persisted === false) {
          // Code-review 5.2: warn instead of silently succeeding.
          msgs.set(id, { text: `${act === "enable" ? "Enabled" : "Disabled"} for this session only — the change was not saved and will be lost when the app restarts.`, err: true });
        } else msgs.delete(id);
        await load();
        return;
      }
      if (act === "remove") {
        if (!confirmRemove.has(id)) {
          confirmRemove.add(id);
          msgs.set(id, { text: "Click the highlighted remove icon again to delete this backend and its group memberships.", err: false });
          paint();
          return;
        }
        confirmRemove.delete(id);
        const r = await deleteBackend(id);
        if (r.error) msgs.set(id, { text: `Remove failed: ${r.error}`, err: true });
        await load();
        return;
      }
      if (act === "toolcall") {
        // The probe persists its outcome on the backend (the card's
        // Tool calls line), so a reload shows the measured state;
        // the probe's own detail rides the card note meanwhile.
        const r = await testBackendToolCall(id);
        if (disposed) return;
        if (r.error) msgs.set(id, { text: `Tool-call test failed: ${r.error}`, err: true });
        else msgs.set(id, { text: `Tool calls: ${r.data.outcome} — ${r.data.detail}`, err: r.data.outcome !== "verified" });
        await load();
        return;
      }
      if (act === "test") {
        const r = await testBackend(id, "Reply with the single word: ready");
        if (disposed) return;
        if (r.error) {
          msgs.set(id, { text: `Test failed: ${r.error}`, err: true });
        } else if (r.data.content) {
          const reply = r.data.content.trim();
          const excerpt = reply.length > 120 ? `${reply.slice(0, 120)}…` : reply;
          msgs.set(id, { text: `Test reply (${r.data.latency_ms} ms): ${excerpt}`, err: false });
        } else {
          msgs.set(id, { text: `Test failed: HTTP ${r.data.status} — no reply content.`, err: true });
        }
        paint();
        return;
      }
    } finally {
      inFlight.delete(id);
      if (!disposed) paint();
    }
  };
  const onKeyDown = (ev: KeyboardEvent): void => {
    if (ev.key === "Escape" && openMenus.size) {
      openMenus.clear();
      paint();
    }
  };
  root.addEventListener("click", onClick);
  root.addEventListener("click", onOvGoto);
  root.addEventListener("keydown", onKeyDown);

  await load();

  // Keep the view's data fresh (a backend can die outside this view).
  // Polling continues during lifecycle actions: the shared store's
  // pending overlay and the server's own transition keep the cards
  // truthful meanwhile (UI-5).
  const timer = window.setInterval(() => {
    if (!disposed) void load();
  }, 5000);
  // Lifecycle state between loads comes from the shared store (fast
  // while anything is in transition, immediate on the backend's
  // lifecycle events): fold its rows' state into the wire rows and
  // repaint only when an effective state actually changed.
  const unsubscribeStore = serviceStore.subscribe((snap: ServiceSnapshot) => {
    if (disposed || !wireRows.length) return;
    const fresh = new Map(snap.backends.map((r) => [r.id, r]));
    wireRows = wireRows.map((r) => {
      const f = fresh.get(r.id);
      return f ? { ...r, state: f.state, lifecycle: f.lifecycle, transition: f.transition ?? null, pid: f.pid, failure_reason: f.failure_reason } : r;
    });
    const next = wireRows.map(effRow);
    if (lcSignature(next) === lcSignature(lastRows)) return;
    lastRows = next;
    const sliderFocused = (document.activeElement as HTMLElement | null)?.hasAttribute?.("data-offload-slider");
    const rawFocused = (document.activeElement as HTMLElement | null)?.hasAttribute?.("data-flags-raw");
    if (!sliderFocused && !rawFocused) paint();
  });

  return () => {
    disposed = true;
    unsubscribeStore();
    window.clearInterval(timer);
    stopPredictTicker();
    root.removeEventListener("click", onClick);
    root.removeEventListener("click", onOvGoto);
    root.removeEventListener("keydown", onKeyDown);
    root.removeEventListener("change", syncEditModel);
    root.removeEventListener("change", syncFlagsDraft);
    root.removeEventListener("input", syncFlagsDraft);
    root.removeEventListener("input", syncOffloadDraft);
    root.removeEventListener("change", commitOffload);
    root.removeEventListener("change", syncPlaceDraft);
    // `root` is the persistent #view container, so every listener
    // wireBackends adds must be removed here or it survives the visit
    // and fires (with stale closures) on every later visit (code
    // review v0.1.60 UI-5).
    root.removeEventListener("input", syncRawDraft);
    root.removeEventListener("change", changeMtp);
    root.removeEventListener("click", clickMoeCacheSwitch);
    root.removeEventListener("change", changeMoeCacheSize);
    root.removeEventListener("change", commitParallel);
    root.removeEventListener("change", changeCache);
    root.removeEventListener("click", clickCacheBest);
    refreshBtn?.removeEventListener("click", onRefresh);
  };
}

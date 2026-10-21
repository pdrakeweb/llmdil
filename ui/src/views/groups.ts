/* Groups view (#/groups) — predicted-throughput design §8.
   The membership editor: one lane per gateway group plus an
   Ungrouped lane, lanes built from the gateway config directly
   (a backend a hand-edited config lists in two groups appears
   in both lanes — the config's truth, not the first-match
   projection list_backends rows carry). Compact backend cards
   show the saved configuration's predicted throughput (§7) and
   placement; host:port members are static chips, never
   draggable. Membership changes are moves (decision D5 — one
   group per backend): drag-and-drop or the card's "Move to"
   menu, each an immediate gateway_set_backend_group op on the
   config-save + Activation Plan path. Group create and
   delete-empty ride the same path (decision D11); rename,
   strategy, fallbacks, and purpose editing are deliberately
   not here (§8.6) — the Gateway page owns the traffic view and
   links here, and this page links back.
   Drag-and-drop requires the Tauri window's OS file-drop
   handler to be OFF (dragDropEnabled: false in
   src-tauri/tauri.conf.json): Tauri enables it by default, and
   on Windows it intercepts every drag in the webview, so no
   lane ever receives a dragover the page can accept — the OS
   shows the no-drop cursor everywhere (Pete, 2026-10-20). The
   app uses no OS file-drop surface, so disabling it loses
   nothing. */

import {
  gatewayCreateGroup, gatewayDeleteGroup, gatewaySetBackendGroup,
  getBackends, getGatewayConfig, getModels, optimizeMeasureBackend,
} from "../api";
import type {
  BackendRow, GatewayConfigData, GatewayGroup, ModelRow,
} from "../api";
import { trim1 } from "../charts";
import { esc } from "../html";
import { icon } from "../icons";
import { predictedCardTip, sameModelFile } from "./static";

const UNGROUPED = "__ungrouped";
const INPUT_STYLE = "background:var(--panel);color:var(--text);border:1px solid var(--border);border-radius:8px;padding:6px 8px;font-size:12px";

function topbar(title: string, extra = ""): string {
  return `<div class="topbar"><h1>${title}</h1>
    <span class="spacer"></span>${extra}</div>`;
}

function loadingHtml(label: string): string {
  return `<div class="card" style="color:var(--text2)">${icon("clock")} ${esc(label)}</div>`;
}

function emptyHtml(text: string): string {
  return `<div class="card" style="color:var(--text2)">${esc(text)}</div>`;
}

/** Elapsed for an in-flight job, in the Optimize view's format
 *  ("45 s", "2 min 5 s"): the predicted-throughput refresh's
 *  Measuring state ticks in this shape. */
function fmtElapsed(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return m > 0 ? `${m} min ${s} s` : `${s} s`;
}

export function renderGroups(): string {
  return `${topbar("Groups", `<a class="btn" href="#/gateway">${icon("gateway")} Gateway</a>
    <button class="btn" data-groups-new>${icon("plus")} New group</button>`)}
  <div class="view"><div id="groupsStatus"></div>
  <div id="groupsForm"></div>
  <div id="groupsBody">${loadingHtml("Loading groups\u2026")}</div></div>`;
}

export async function wireGroups(root: HTMLElement): Promise<() => void> {
  let disposed = false;
  let rows: BackendRow[] = [];
  let groups: GatewayGroup[] = [];
  let models: ModelRow[] = [];
  let gatewayCfg: GatewayConfigData | null = null;
  let loaded = false;
  let loadError = "";
  /** Backend ids whose "Move to" menu is open (at most one per
   *  card instance; state lives outside the repainted DOM so a
   *  poll repaint cannot close an open menu). */
  const openMenus = new Set<string>();
  /** Group names whose Delete button is armed for confirmation. */
  const confirmDelete = new Set<string>();
  /** Backends whose predicted-throughput refresh (a Measure
   *  run started from the mini card) is in flight, by start
   *  time; state lives outside the repainted DOM so poll
   *  repaints keep the Measuring state. */
  const predictRefreshing = new Map<string, number>();
  /** The last refresh failure per backend, rendered on its
   *  card until a refresh succeeds — a failure is visible on
   *  the card, never silent. */
  const predictNotes = new Map<string, string>();
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
      root.querySelectorAll<HTMLElement>("[data-group-pred-elapsed]").forEach((el) => {
        const startedAt = predictRefreshing.get(el.getAttribute("data-group-pred-elapsed") ?? "");
        if (startedAt !== undefined) {
          el.textContent = fmtElapsed(Math.max(0, Math.floor((Date.now() - startedAt) / 1000)));
        }
      });
    }, 1000);
  };
  /** The move in flight, applied optimistically over the last
   *  fetched membership until the op lands. */
  let pending: { id: string; to: string | null } | null = null;
  /** Backend id of the card being dragged (dataTransfer cannot
   *  be read during dragover, and the drop handler needs it). */
  let dragId: string | null = null;
  /** A repaint requested while a drag is in flight is deferred
   *  until the drag ends: paint() replaces #groupsBody's DOM,
   *  which would destroy the drag source node mid-drag and
   *  cancel the drag. Applied by onDragEnd / onDrop. */
  let paintDeferredDuringDrag = false;
  let status: { text: string; err: boolean } | null = null;
  let formOpen = false;
  let formError = "";
  /** The create form's draft name, held outside the repainted
   *  DOM (design rule 17): paintForm re-renders the input from
   *  this draft, so a failed create — and the 10 s poll repaint —
   *  keep the operator's typed name intact instead of clearing
   *  the field. Cleared only when the form closes. */
  let formDraft = "";

  const bodyEl = (): HTMLElement | null => root.querySelector("#groupsBody");
  const statusEl = (): HTMLElement | null => root.querySelector("#groupsStatus");
  const formEl = (): HTMLElement | null => root.querySelector("#groupsForm");

  const configuredRows = (): BackendRow[] => rows.filter((r) => r.configured);
  const byId = (): Map<string, BackendRow> =>
    new Map(configuredRows().map((r) => [r.id, r]));

  /** Membership as the config states it (before any optimistic
   *  pending move): the group names whose member list holds id. */
  const membershipsOf = (id: string): string[] =>
    groups.filter((g) => g.members.includes(id)).map((g) => g.name);
  /** Membership as rendered: the pending move, when one is in
   *  flight for this backend, replaces the config's answer. */
  const effMembershipsOf = (id: string): string[] =>
    pending && pending.id === id
      ? (pending.to ? [pending.to] : [])
      : membershipsOf(id);

  const modelName = (b: BackendRow): string => {
    const m = models.find((x) => sameModelFile(b.model_file, x.gguf_path));
    return m ? m.name : b.model_file.split(/[\\/]/).pop() ?? b.model_file;
  };

  const dotClass = (state: string): string =>
    state === "running" ? "ok"
      : state === "failed" ? "err"
        : state === "starting" || state === "stopping" ? "warn"
          : "idle";

  /* Exclusion states, mirroring the Gateway view's group cards:
   * a member or fallback renders muted with the reason when the
   * generated gateway config would drop it. */
  const memberState = (name: string): { excluded: boolean; reason: string } => {
    if (name === "openrouter") {
      return gatewayCfg?.openrouter && !gatewayCfg.openrouter.enabled
        ? { excluded: true, reason: "tier disabled" }
        : { excluded: false, reason: "" };
    }
    const b = byId().get(name);
    return b && !b.enabled
      ? { excluded: true, reason: "backend disabled" }
      : { excluded: false, reason: "" };
  };
  const fallbackState = (name: string): { excluded: boolean; reason: string } => {
    const g = groups.find((x) => x.name === name);
    if (g && name !== "openrouter") {
      return g.members.length === 0 || g.members.every((m) => memberState(m).excluded)
        ? { excluded: true, reason: "no active members" }
        : { excluded: false, reason: "" };
    }
    return memberState(name);
  };

  const cardHtml = (b: BackendRow, laneKey: string): string => {
    const menuKey = `${laneKey}:${b.id}`;
    const menuOpen = openMenus.has(menuKey);
    const destinations = [
      ...groups.map((g) => g.name).filter((n) => n !== laneKey),
      ...(laneKey !== UNGROUPED ? [null] : []),
    ];
    const menu = destinations.length
      ? `<div class="advanced-menu-wrap" data-group-menu-wrap>
      <button class="btn icon-only" data-group-menu="${esc(menuKey)}" aria-haspopup="menu" aria-expanded="${menuOpen}" title="Move ${esc(b.id)} to another group" aria-label="Move ${esc(b.id)} to another group">${icon("menu")}</button>
      ${menuOpen ? `<div class="advanced-menu" role="menu" aria-label="Move ${esc(b.id)} to">
        ${destinations.map((d) => `<button class="menu-item" role="menuitem" data-group-move="${esc(b.id)}" data-group-to="${esc(d ?? "")}">${icon("layers")} ${d === null ? "Ungrouped" : esc(d)}</button>`).join("")}
      </div>` : ""}
    </div>`
      : "";
    // Predicted throughput renders on EVERY card (Pete,
    // 2026-10-20): the figure when an exact measurement
    // resolves, the explicit "Not measured" empty state when
    // none does (never 0, never an omitted line), and the
    // Refresh control beside it — the same Measure dispatch
    // the Backends card's Refresh makes. Measuring requires a
    // running backend (the measure op's own rule), so a
    // stopped backend's control is disabled with the reason
    // as its title.
    const fig = b.predicted && b.predicted.predicted_per_second !== null
      ? b.predicted.predicted_per_second
      : null;
    const pStarted = predictRefreshing.get(b.id);
    const pCanMeasure = b.state === "running";
    const pTitle = pStarted !== undefined
      ? "Measurement running"
      : !pCanMeasure
        ? "Backend is not running — start it to refresh the prediction"
        : "Refresh the prediction — runs the standardized benchmark";
    const pBtn = `<button class="btn" data-group-predict-refresh="${esc(b.id)}" style="padding:2px 8px;font-size:11px" title="${esc(pTitle)}" aria-label="Refresh predicted throughput for ${esc(b.id)}"${pStarted !== undefined || !pCanMeasure ? " disabled" : ""}>${icon("restart")} Refresh</button>`;
    const pNote = predictNotes.get(b.id);
    const predicted = `<div class="group-mini-line">Predicted throughput: ${pStarted !== undefined
      ? `Measuring <span data-group-pred-elapsed="${esc(b.id)}">${fmtElapsed(Math.max(0, Math.floor((Date.now() - pStarted) / 1000)))}</span>`
      : fig !== null
        ? `<span style="color:var(--text)">${trim1(fig)} tok/s</span><span class="info-tip" title="${esc(predictedCardTip(b.predicted!))}">${icon("info")}</span>`
        : `<span style="color:var(--text3)">Not measured</span>`} ${pBtn}</div>${pNote ? `<div class="group-mini-line" style="color:var(--err)">${esc(pNote)}</div>` : ""}`;
    const isPending = pending !== null && pending.id === b.id;
    return `<div class="group-mini-card" draggable="true" data-group-card="${esc(b.id)}" data-group-lane="${esc(laneKey)}">
      <div class="group-mini-head"><span class="dot ${dotClass(b.state)}"></span><span class="group-mini-id">${esc(b.id)}</span><span style="color:var(--text3);font-family:var(--mono);font-size:12px">:${b.port}</span>${menu}</div>
      <div class="group-mini-line">Model: <span style="color:var(--text)">${esc(modelName(b))}</span></div>
      ${predicted}
      ${b.placement ? `<div class="group-mini-line">Placement: <span style="color:var(--text)">${esc(b.placement)}</span></div>` : ""}
      ${isPending ? `<div class="group-mini-line" style="color:var(--warn)">Moving\u2026</div>` : ""}
    </div>`;
  };

  const laneHtml = (g: GatewayGroup | null): string => {
    const laneKey = g ? g.name : UNGROUPED;
    const ids = byId();
    const memberIds: string[] = [];
    if (g) {
      for (const m of g.members) {
        if (ids.has(m) && effMembershipsOf(m).includes(g.name)) memberIds.push(m);
      }
      // The pending move's target lane shows the card before the
      // refetch lands (optimistic), appended after the members
      // the config already lists.
      if (pending && pending.to === g.name && !memberIds.includes(pending.id) && ids.has(pending.id)) {
        memberIds.push(pending.id);
      }
    } else {
      for (const r of configuredRows()) {
        if (effMembershipsOf(r.id).length === 0) memberIds.push(r.id);
      }
    }
    const externals = g ? g.members.filter((m) => !ids.has(m)) : [];
    const fallbacks = g && g.fallbacks.length
      ? `<div style="font-size:12px;margin-top:2px"><span style="color:var(--text2)">Fallbacks:</span> ${g.fallbacks.map((name) => {
          const st = fallbackState(name);
          const tip = st.reason === "no active members"
            ? "No active members \u2014 dropped from the generated gateway config"
            : "Disabled \u2014 dropped from the generated gateway config";
          return `<span style="color:${st.excluded ? "var(--text3)" : "#3fb950"}"${st.excluded ? ` title="${tip}"` : ""}>${esc(name)}</span>`;
        }).join(" \u00b7 ")}</div>`
      : "";
    const deleteBtn = g && g.members.length === 0
      ? `<button class="btn danger" data-group-delete="${esc(g.name)}" style="margin-left:auto">${icon("trash")} ${confirmDelete.has(g.name) ? "Confirm delete" : "Delete group"}</button>`
      : "";
    const head = g
      ? `<span class="group-lane-name">${esc(g.name)}</span>
        <span class="chip">${esc(g.strategy)}</span>
        ${g.enabled ? "" : `<span class="chip status-tag status-off" title="No enabled backends \u2014 omitted from the generated gateway config">disabled</span>`}
        <span style="font-size:12px;color:var(--text2)">Members: ${g.members.length}</span>${deleteBtn}`
      : `<span class="group-lane-name">Ungrouped</span>
        <span style="font-size:12px;color:var(--text2)">Members: ${memberIds.length}</span>`;
    return `<section class="group-lane" data-group-lane-drop="${esc(laneKey)}">
      <div class="group-lane-head">${head}</div>
      ${fallbacks}
      <div class="group-lane-body">
        ${memberIds.map((id) => cardHtml(ids.get(id)!, laneKey)).join("")}
        ${externals.map((m) => `<div class="group-ext-chip"><span class="chip" style="color:var(--text3)" title="External endpoint \u2014 not a local backend">${esc(m)}</span></div>`).join("")}
        ${memberIds.length === 0 && externals.length === 0 ? `<div class="group-drop-hint">Drop backends here</div>` : ""}
      </div>
    </section>`;
  };

  function paintStatus(): void {
    const el = statusEl();
    if (!el) return;
    el.innerHTML = status
      ? `<div style="font-size:12px;margin-bottom:10px;color:${status.err ? "var(--err)" : "var(--text2)"}">${esc(status.text)}</div>`
      : "";
  }

  function paintForm(): void {
    const el = formEl();
    if (!el) return;
    el.innerHTML = formOpen
      ? `<div class="card" style="margin-bottom:12px"><div class="sec-title">${icon("plus")} New group</div>
        <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap">
          <input id="groupsNewName" type="text" placeholder="group-name" value="${esc(formDraft)}" style="${INPUT_STYLE}" autocomplete="off">
          <button class="btn" data-group-create>${icon("check")} Create group</button>
          <button class="btn" data-group-create-cancel>Cancel</button>
        </div>
        ${formError ? `<div style="font-size:12px;color:var(--err);margin-top:6px">${esc(formError)}</div>` : ""}
        <div style="font-size:12px;color:var(--text3);margin-top:6px">Lowercase letters, digits, and hyphens.</div>
      </div>`
      : "";
    if (formOpen) root.querySelector<HTMLInputElement>("#groupsNewName")?.focus();
  }

  function paint(): void {
    paintStatus();
    paintForm();
    const el = bodyEl();
    if (!el) return;
    // Never replace the lanes DOM while a drag is in flight —
    // the dragged card is inside it, and destroying the drag
    // source cancels the drag (see paintDeferredDuringDrag).
    if (dragId !== null) { paintDeferredDuringDrag = true; return; }
    if (!loaded) { el.innerHTML = loadingHtml("Loading groups\u2026"); return; }
    if (loadError) { el.innerHTML = emptyHtml(`Groups unavailable: ${loadError}`); return; }
    el.innerHTML = `<div class="groups-lanes">${[...groups.map((g) => laneHtml(g)), laneHtml(null)].join("")}</div>`;
  }

  async function load(): Promise<void> {
    const [b, gw, m] = await Promise.all([getBackends(), getGatewayConfig(), getModels()]);
    if (disposed) return;
    if (b.error || gw.error || m.error) {
      loadError = b.error ?? gw.error ?? m.error ?? "unknown error";
      loaded = true;
      paint();
      return;
    }
    loadError = "";
    rows = b.data;
    gatewayCfg = gw.data;
    groups = gw.data?.groups ?? [];
    models = m.data;
    loaded = true;
    paint();
  }

  function setStatus(text: string, err = false): void {
    status = { text, err };
    paintStatus();
  }

  /** Refresh one card's predicted throughput: a Measure run
   *  against the running backend through the existing Optimize
   *  job machinery (the Measure profile — the one whose entries
   *  feed the prediction; never the Estimate profile). The
   *  card shows the Measuring state while the invoke is
   *  outstanding, updates from the refreshed rows on
   *  completion, and carries the failure on the card when the
   *  run fails or is refused. */
  async function doPredictRefresh(id: string): Promise<void> {
    if (predictRefreshing.has(id)) return;
    const row = byId().get(id);
    if (!row || row.state !== "running") return;
    predictRefreshing.set(id, Date.now());
    predictNotes.delete(id);
    paint();
    startPredictTicker();
    const r = await optimizeMeasureBackend(id);
    predictRefreshing.delete(id);
    if (!predictRefreshing.size) stopPredictTicker();
    if (disposed) return;
    if (r.error) {
      predictNotes.set(id, r.error === "measurement cancelled"
        ? "Prediction refresh cancelled."
        : `Prediction refresh failed — ${r.error}`);
      paint();
      return;
    }
    predictNotes.delete(id);
    await load();
  }

  async function doMove(id: string, to: string | null): Promise<void> {
    openMenus.clear();
    const current = membershipsOf(id);
    // A drop on the lane the backend solely occupies (or an
    // ungroup of an ungrouped backend) is a no-op by design
    // (§8.4) — say so without a config write.
    const already = to === null
      ? current.length === 0
      : current.length === 1 && current[0] === to;
    if (already) {
      setStatus(to === null
        ? `Backend ${id} is already ungrouped.`
        : `Backend ${id} is already in group ${to}.`);
      paint();
      return;
    }
    pending = { id, to };
    paint();
    const r = await gatewaySetBackendGroup(id, to);
    if (disposed) return;
    pending = null;
    if (r.error) {
      // The card returns to its source lane on the repaint —
      // the render still reads the last fetched membership.
      setStatus(`Move failed: ${r.error}`, true);
      paint();
      return;
    }
    const removed = (r.data.removed_from ?? []).filter((n) => n !== to);
    const collapse = removed.length
      ? ` It was also removed from: ${removed.join(", ")}.`
      : "";
    setStatus(to === null
      ? `Backend ${id} is now ungrouped.${collapse}`
      : `Backend ${id} moved to group ${to}.${collapse}`);
    await load();
  }

  async function doCreate(): Promise<void> {
    const input = root.querySelector<HTMLInputElement>("#groupsNewName");
    const name = (input?.value ?? formDraft).trim();
    if (!name) { formError = "Enter a group name."; paintForm(); return; }
    const r = await gatewayCreateGroup(name);
    if (disposed) return;
    if (r.error) { formError = r.error; paintForm(); return; }
    // Success (rule 17): the form closes and the new group lane
    // displays in its new state through load().
    formOpen = false;
    formError = "";
    formDraft = "";
    setStatus(`Group ${name} created.`);
    await load();
  }

  async function doDelete(name: string): Promise<void> {
    confirmDelete.delete(name);
    const r = await gatewayDeleteGroup(name);
    if (disposed) return;
    if (r.error) { setStatus(`Delete failed: ${r.error}`, true); paint(); return; }
    setStatus(`Group ${name} deleted.`);
    await load();
  }

  function onClick(ev: Event): void {
    if (disposed) return;
    const t = ev.target as HTMLElement;
    if (t.closest("[data-groups-new]")) {
      formOpen = !formOpen;
      formError = "";
      if (!formOpen) formDraft = "";
      paintForm();
      return;
    }
    const pr = t.closest("[data-group-predict-refresh]") as HTMLElement | null;
    if (pr) { void doPredictRefresh(pr.getAttribute("data-group-predict-refresh")!); return; }
    if (t.closest("[data-group-create]")) { void doCreate(); return; }
    if (t.closest("[data-group-create-cancel]")) {
      formOpen = false;
      formError = "";
      formDraft = "";
      paintForm();
      return;
    }
    const del = t.closest("[data-group-delete]") as HTMLElement | null;
    if (del) {
      const name = del.getAttribute("data-group-delete")!;
      if (confirmDelete.has(name)) { void doDelete(name); return; }
      confirmDelete.clear();
      confirmDelete.add(name);
      paint();
      return;
    }
    const menuBtn = t.closest("[data-group-menu]") as HTMLElement | null;
    if (menuBtn) {
      const key = menuBtn.getAttribute("data-group-menu")!;
      if (openMenus.has(key)) openMenus.delete(key);
      else { openMenus.clear(); openMenus.add(key); }
      paint();
      return;
    }
    const move = t.closest("[data-group-move]") as HTMLElement | null;
    if (move) {
      const id = move.getAttribute("data-group-move")!;
      const toRaw = move.getAttribute("data-group-to");
      void doMove(id, toRaw ? toRaw : null);
      return;
    }
    // A click outside any menu closes the open one.
    if (openMenus.size && !t.closest("[data-group-menu-wrap]")) {
      openMenus.clear();
      paint();
    }
  }

  function onKeydown(ev: KeyboardEvent): void {
    if (disposed) return;
    if ((ev.target as HTMLElement).id === "groupsNewName" && ev.key === "Enter") {
      ev.preventDefault();
      void doCreate();
    }
  }

  function clearDropHighlights(): void {
    root.querySelectorAll(".group-lane.dragover")
      .forEach((el) => el.classList.remove("dragover"));
  }

  /** The lane element an event landed in, or null. The drop
   *  attribute is carried by the lane <section> itself, so
   *  closest() resolves a hit anywhere in the lane — header,
   *  cards, fallbacks line, empty space, the drop hint. The
   *  target may be an SVG element inside an icon (its closest
   *  exists but is not an HTMLElement), so resolve through
   *  Element, and tolerate a target with no closest at all. */
  function laneFromEvent(ev: Event): HTMLElement | null {
    const t = ev.target as Element | null;
    if (!t || typeof t.closest !== "function") return null;
    return t.closest("[data-group-lane-drop]") as HTMLElement | null;
  }

  /** Whether a drag this page started is in flight. dragId is
   *  the primary record; the dataTransfer type list is the
   *  fallback (readable during dragover even though the data
   *  itself is not) so an accepted drag stays accepted even if
   *  module state was disturbed mid-drag. File drags carry
   *  "Files", never "text/plain", so they are not accepted. */
  function dragInFlight(ev: DragEvent): boolean {
    if (dragId !== null) return true;
    try {
      return ev.dataTransfer?.types.includes("text/plain") ?? false;
    } catch {
      return false;
    }
  }

  function applyDeferredPaint(): void {
    if (!paintDeferredDuringDrag) return;
    paintDeferredDuringDrag = false;
    paint();
  }

  function onDragStart(ev: DragEvent): void {
    if (disposed) return;
    const t = ev.target as Element | null;
    const card = t && typeof t.closest === "function"
      ? t.closest("[data-group-card]") as HTMLElement | null
      : null;
    if (!card) return;
    dragId = card.getAttribute("data-group-card");
    paintDeferredDuringDrag = false;
    if (ev.dataTransfer && dragId) {
      ev.dataTransfer.setData("text/plain", dragId);
      ev.dataTransfer.effectAllowed = "move";
    }
  }

  function onDragOver(ev: DragEvent): void {
    if (disposed || !dragInFlight(ev)) return;
    const lane = laneFromEvent(ev);
    if (!lane) return;
    // preventDefault is what makes the lane a drop target —
    // without it on every dragover the browser keeps the
    // no-drop cursor and no drop event ever fires.
    ev.preventDefault();
    if (ev.dataTransfer) ev.dataTransfer.dropEffect = "move";
    clearDropHighlights();
    lane.classList.add("dragover");
  }

  function onDragLeave(ev: DragEvent): void {
    if (disposed) return;
    const lane = laneFromEvent(ev);
    if (!lane) return;
    // Moving between children of the same lane is not leaving
    // it — only clear when the drag actually exits the lane, so
    // the highlight does not flicker crossing card boundaries.
    const next = ev.relatedTarget as Node | null;
    if (next && lane.contains(next)) return;
    lane.classList.remove("dragover");
  }

  function onDrop(ev: DragEvent): void {
    if (disposed) return;
    const lane = laneFromEvent(ev);
    const id = dragId ?? ev.dataTransfer?.getData("text/plain") ?? null;
    dragId = null;
    clearDropHighlights();
    if (!lane || !id) { applyDeferredPaint(); return; }
    ev.preventDefault();
    paintDeferredDuringDrag = false;
    const key = lane.getAttribute("data-group-lane-drop")!;
    void doMove(id, key === UNGROUPED ? null : key);
  }

  function onDragEnd(): void {
    dragId = null;
    clearDropHighlights();
    applyDeferredPaint();
  }

  /** Keep the create form's draft in step with the input, so a
   *  repaint (poll or a failed create's error render) restores
   *  the typed name from formDraft instead of losing it. */
  function onInput(ev: Event): void {
    const t = ev.target as HTMLElement;
    if (t.id === "groupsNewName") formDraft = (t as HTMLInputElement).value;
  }

  root.addEventListener("click", onClick);
  root.addEventListener("input", onInput);
  root.addEventListener("keydown", onKeydown);
  root.addEventListener("dragstart", onDragStart);
  root.addEventListener("dragover", onDragOver);
  root.addEventListener("dragleave", onDragLeave);
  root.addEventListener("drop", onDrop);
  root.addEventListener("dragend", onDragEnd);
  paint();
  void load();
  const timer = window.setInterval(() => {
    if (!disposed && !pending && !dragId) void load();
  }, 10000);
  return () => {
    disposed = true;
    window.clearInterval(timer);
    stopPredictTicker();
  };
}

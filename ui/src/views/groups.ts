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
   links here, and this page links back. */

import {
  gatewayCreateGroup, gatewayDeleteGroup, gatewaySetBackendGroup,
  getBackends, getGatewayConfig, getModels,
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
  /** The move in flight, applied optimistically over the last
   *  fetched membership until the op lands. */
  let pending: { id: string; to: string | null } | null = null;
  /** Backend id of the card being dragged (dataTransfer cannot
   *  be read during dragover, and the drop handler needs it). */
  let dragId: string | null = null;
  let status: { text: string; err: boolean } | null = null;
  let formOpen = false;
  let formError = "";

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
    const predicted = b.predicted && b.predicted.predicted_per_second !== null
      ? `<div class="group-mini-line">Predicted throughput: <span style="color:var(--text)">${trim1(b.predicted.predicted_per_second)} tok/s</span><span class="info-tip" title="${esc(predictedCardTip(b.predicted))}">${icon("info")}</span></div>`
      : "";
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
          <input id="groupsNewName" type="text" placeholder="group-name" style="${INPUT_STYLE}" autocomplete="off">
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
    const name = (input?.value ?? "").trim();
    if (!name) { formError = "Enter a group name."; paintForm(); return; }
    const r = await gatewayCreateGroup(name);
    if (disposed) return;
    if (r.error) { formError = r.error; paintForm(); return; }
    formOpen = false;
    formError = "";
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
      paintForm();
      return;
    }
    if (t.closest("[data-group-create]")) { void doCreate(); return; }
    if (t.closest("[data-group-create-cancel]")) {
      formOpen = false;
      formError = "";
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

  function onDragStart(ev: DragEvent): void {
    if (disposed) return;
    const card = (ev.target as HTMLElement).closest("[data-group-card]") as HTMLElement | null;
    if (!card) return;
    dragId = card.getAttribute("data-group-card");
    if (ev.dataTransfer && dragId) {
      ev.dataTransfer.setData("text/plain", dragId);
      ev.dataTransfer.effectAllowed = "move";
    }
  }

  function onDragOver(ev: DragEvent): void {
    if (disposed || !dragId) return;
    const lane = (ev.target as HTMLElement).closest("[data-group-lane-drop]") as HTMLElement | null;
    if (!lane) return;
    ev.preventDefault();
    if (ev.dataTransfer) ev.dataTransfer.dropEffect = "move";
    clearDropHighlights();
    lane.classList.add("dragover");
  }

  function onDrop(ev: DragEvent): void {
    if (disposed) return;
    const lane = (ev.target as HTMLElement).closest("[data-group-lane-drop]") as HTMLElement | null;
    const id = dragId ?? ev.dataTransfer?.getData("text/plain") ?? null;
    dragId = null;
    clearDropHighlights();
    if (!lane || !id) return;
    ev.preventDefault();
    const key = lane.getAttribute("data-group-lane-drop")!;
    void doMove(id, key === UNGROUPED ? null : key);
  }

  function onDragEnd(): void {
    dragId = null;
    clearDropHighlights();
  }

  root.addEventListener("click", onClick);
  root.addEventListener("keydown", onKeydown);
  root.addEventListener("dragstart", onDragStart);
  root.addEventListener("dragover", onDragOver);
  root.addEventListener("drop", onDrop);
  root.addEventListener("dragend", onDragEnd);
  paint();
  void load();
  const timer = window.setInterval(() => {
    if (!disposed && !pending) void load();
  }, 10000);
  return () => {
    disposed = true;
    window.clearInterval(timer);
  };
}

/* Logs view (split from static.ts). */

import { exportDiagnostics, getBackends, getLogs, openFolder } from "../api";
import type { LogRow } from "../api";
import { fmtCount, meter } from "../charts";
import { esc } from "../html";
import { icon } from "../icons";
import { C, SELECT_STYLE, bannerHtml, emptyHtml, fmtTime, loadingHtml, setBanner, topbar } from "./common";

/* ---------------- Logs ---------------- */

/** Logs view control state (tracker #59): everything the filter
 *  card holds, kept at module level so leaving the view and coming
 *  back within the same app run restores it exactly as left. An
 *  app restart re-creates the module and returns to the defaults
 *  below. While the view is wired the controls are the runtime
 *  truth; wireLogs mirrors them into this object on every change
 *  and once more from its cleanup. */
interface LogsViewState {
  scope: string;
  level: string;
  query: string;
  from: string;
  to: string;
  sort: "asc" | "desc";
  limit: number;
  follow: boolean;
  page: number;
}

function defaultLogsViewState(): LogsViewState {
  return { scope: "all", level: "", query: "", from: "", to: "", sort: "desc", limit: 200, follow: false, page: 1 };
}

let logsViewState: LogsViewState = defaultLogsViewState();

/** One recorded Logs search set (tracker #59): an applied query —
 *  every getLogs parameter except the pagination offset — plus
 *  when it last ran. Follow is view state, not part of a set:
 *  applying a set never changes the Follow toggle. */
interface LogSearchSet {
  scope: string;
  level: string;
  query: string;
  from: string;
  to: string;
  sort: "asc" | "desc";
  limit: number;
  ranAt: string;
}

const LOGS_RECENTS_KEY = "llm.logs.recents";
const LOGS_RECENTS_CAP = 30;

/** Recents persist across restarts in localStorage (the dashboard
 *  view-mode pattern): 30 small entries at most. Stored data is
 *  validated field by field; any storage problem degrades to an
 *  empty history rather than breaking the view. */
function loadLogsRecents(): LogSearchSet[] {
  try {
    const raw = localStorage.getItem(LOGS_RECENTS_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    const out: LogSearchSet[] = [];
    for (const e of parsed) {
      if (!e || typeof e !== "object") continue;
      const r = e as Record<string, unknown>;
      if (typeof r.scope !== "string" || typeof r.ranAt !== "string") continue;
      out.push({
        scope: r.scope,
        level: typeof r.level === "string" ? r.level : "",
        query: typeof r.query === "string" ? r.query : "",
        from: typeof r.from === "string" ? r.from : "",
        to: typeof r.to === "string" ? r.to : "",
        sort: r.sort === "asc" ? "asc" : "desc",
        limit: typeof r.limit === "number" && [50, 200, 500].includes(r.limit) ? r.limit : 200,
        ranAt: r.ranAt,
      });
      if (out.length >= LOGS_RECENTS_CAP) break;
    }
    return out;
  } catch { return []; }
}

let logsRecents: LogSearchSet[] = loadLogsRecents();
/** Whether the Recent searches section is expanded — session-only
 *  view state. The section itself renders only while the history is
 *  non-empty (principle 13). */
let logsRecentsOpen = false;

function saveLogsRecents(): void {
  try { localStorage.setItem(LOGS_RECENTS_KEY, JSON.stringify(logsRecents)); } catch { /* storage unavailable */ }
}

/** A set's identity: the query fields only — ranAt is metadata, so
 *  re-running the same set dedupes instead of duplicating. */
function logSearchKey(s: LogSearchSet): string {
  return JSON.stringify([s.scope, s.level, s.query, s.from, s.to, s.sort, s.limit]);
}

/** Record an applied search set: newest first; an identical set
 *  moves to the top with a fresh ranAt; the oldest drop past the
 *  cap. */
function recordLogSearch(set: LogSearchSet): void {
  const key = logSearchKey(set);
  logsRecents = [set, ...logsRecents.filter((e) => logSearchKey(e) !== key)].slice(0, LOGS_RECENTS_CAP);
  saveLogsRecents();
}

/** One-line summary of a search set: the scope, then only the
 *  distinguishing values (defaults omitted), so entries scan. */
function logSearchSummary(s: LogSearchSet): string {
  const parts: string[] = [s.scope];
  if (s.level) parts.push(`level ${s.level}`);
  if (s.query) parts.push(`"${s.query}"`);
  if (s.from || s.to) parts.push(`${s.from ? fmtRanAt(s.from) : "…"} → ${s.to ? fmtRanAt(s.to) : "…"}`);
  if (s.sort === "asc") parts.push("oldest first");
  if (s.limit !== 200) parts.push(`limit ${s.limit}`);
  return parts.join(" · ");
}

/** A timestamp compactly — "18 Oct 22:14" (the full stamp rides
 *  the title attribute where one is offered). Accepts ISO stamps
 *  and datetime-local values alike. */
function fmtRanAt(ts: string): string {
  const d = new Date(ts);
  return Number.isNaN(d.getTime())
    ? ts
    : d.toLocaleString(undefined, { day: "2-digit", month: "short", hour: "2-digit", minute: "2-digit" });
}

export function fmtDateTime(ts: string): string {
  const d = new Date(ts);
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleString(undefined);
}

/** One log line: visible time stays HH:MM:SS, the full date-time is on
 *  the title attribute. In the merged "all" scope the source process is
 *  chipped per line; previous-run (archived) lines are marked. */
function logRow(r: LogRow, showSource: boolean): string {
  const src = showSource && r.source ? ` <span class="chip">${esc(r.source)}</span>` : "";
  const arch = r.archived ? ` <span class="chip badge-warn">previous run</span>` : "";
  // Lifecycle markers ("[lifecycle] … restarting (origin: …)") are
  // what every service's own log scope records for start / stop /
  // restart / exit — styled so they stand out in a busy stream.
  const lc = r.line.includes("[lifecycle]") ? " log-lifecycle" : "";
  return `<div class="ln${lc}"><span class="t" title="${esc(fmtDateTime(r.ts))}">${esc(fmtTime(r.ts))}</span>${src} <span class="chip">${esc(r.stream)}</span>${arch} ${esc(r.line)}</div>`;
}

export function renderLogs(): string {
  return `${topbar("Logs")}
  <div class="view"><div id="logBanner"></div>
  <div class="card" style="margin-bottom:12px"><div style="display:flex;gap:10px;align-items:center;flex-wrap:wrap">
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Scope<span class="info-tip" title="Backend logs are kept in memory for this session only — up to 10,000 lines per process; one previous run per process is kept. The gateway log is file-backed (under the data directory's logs folder) and survives restarts.">${icon("info")}</span>
      <select id="logScope" style="${SELECT_STYLE}"><option value="all">all</option></select></label>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px" title="Level matches line text \u2014 supervised processes do not emit structured levels">Level
      <select id="logLevel" style="${SELECT_STYLE}">
        <option value="">any</option>
        <option value="error">error</option>
        <option value="warn">warn</option>
        <option value="info">info</option>
        <option value="debug">debug</option>
      </select></label>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Search
      <input id="logSearch" type="text" placeholder="substring\u2026" style="${SELECT_STYLE};min-width:160px"/></label>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">From
      <input id="logFrom" type="datetime-local" style="${SELECT_STYLE}"/></label>
    <button class="btn" id="logFromCal" type="button" title="Pick a date and time" aria-label="Pick a date and time" style="padding:4px 7px">${icon("calendar")}</button>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">To
      <input id="logTo" type="datetime-local" style="${SELECT_STYLE}"/></label>
    <button class="btn" id="logToCal" type="button" title="Pick a date and time" aria-label="Pick a date and time" style="padding:4px 7px">${icon("calendar")}</button>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Sort
      <select id="logSort" style="${SELECT_STYLE}">
        <option value="desc" selected>Newest first</option>
        <option value="asc">Oldest first</option>
      </select></label>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">
      <input id="logFollow" type="checkbox"/> Follow</label>
  </div>
  <div id="logMsg" style="font-size:12px;margin-top:6px"></div>
  <div style="display:flex;gap:10px;align-items:center;flex-wrap:wrap;border-top:1px solid var(--border);margin-top:10px;padding-top:10px">
    <button class="btn icon-only" id="logPrev" title="Previous page" aria-label="Previous page">${icon("chevL")}</button>
    <button class="btn icon-only" id="logNext" title="Next page" aria-label="Next page">${icon("chevR")}</button>
    <span id="logPageInfo" style="font-size:12px;color:var(--text2)"></span>
    <span id="logPosBar" style="display:inline-block;width:64px"></span>
    <span id="logMatchInfo" style="font-size:12px;color:var(--text2)"></span>
    <span class="spacer" style="flex:1"></span>
    <label style="font-size:12px;color:var(--text2);display:inline-flex;align-items:center;gap:6px">Limit
      <select id="logLimit" style="${SELECT_STYLE}">
        <option value="50">50</option>
        <option value="200" selected>200</option>
        <option value="500">500</option>
      </select></label>
    <button class="btn icon-only" id="logRefresh" title="Refresh" aria-label="Refresh">${icon("restart")}</button>
    <button class="btn icon-only" id="logExport" title="Export diagnostics bundle — the bundle contains raw, unredacted process logs. Review it before sharing." aria-label="Export diagnostics bundle">${icon("download")}</button>
    <button class="btn icon-only" id="logOpenFolder" title="Open folder" aria-label="Open folder" style="display:none">${icon("folder")}</button>
  </div></div>
  <div id="logRecents"></div>
  <div id="logBody">${loadingHtml("Loading logs\u2026")}</div></div>`;
}

export async function wireLogs(root: HTMLElement, initialScope?: string, initialFollow = false): Promise<() => void> {
  let disposed = false;
  /** Load coalescing (code-review 5.1): every load takes a
   *  sequence number and only the LATEST load paints. The previous
   *  boolean in-flight guard silently DROPPED a page/filter change
   *  made while a load was running — the view kept showing the old
   *  page under the new filters until some later action. This is
   *  the same pattern the Backends view uses. */
  let loadSeq = 0;
  let followTimer: number | null = null;
  let searchTimer: number | null = null;
  /** 1-based page into the filtered result set; the limit select is
   *  the page size. Any filter change resets this to 1; Refresh and
   *  the Follow re-poll preserve it. */
  let page = 1;
  const scopeSel = root.querySelector("#logScope") as HTMLSelectElement | null;
  const levelSel = root.querySelector("#logLevel") as HTMLSelectElement | null;
  const searchInput = root.querySelector("#logSearch") as HTMLInputElement | null;
  const fromInput = root.querySelector("#logFrom") as HTMLInputElement | null;
  const toInput = root.querySelector("#logTo") as HTMLInputElement | null;
  const sortSel = root.querySelector("#logSort") as HTMLSelectElement | null;
  const limitSel = root.querySelector("#logLimit") as HTMLSelectElement | null;
  const followChk = root.querySelector("#logFollow") as HTMLInputElement | null;
  const refreshBtn = root.querySelector("#logRefresh");
  const prevBtn = root.querySelector("#logPrev") as HTMLButtonElement | null;
  const nextBtn = root.querySelector("#logNext") as HTMLButtonElement | null;
  const pageInfo = root.querySelector("#logPageInfo");
  const matchInfo = root.querySelector("#logMatchInfo");
  const posBar = root.querySelector("#logPosBar") as HTMLElement | null;
  const exportBtn = root.querySelector("#logExport") as HTMLButtonElement | null;

  /** A datetime-local value as epoch ms (local time); empty or
   *  unparseable means unbounded. */
  const msOf = (v: string | undefined): number | undefined => {
    if (!v) return undefined;
    const ms = new Date(v).getTime();
    return Number.isNaN(ms) ? undefined : ms;
  };

  /** The "To" bound is minute-precision: it covers the whole
   *  picked minute, not just its first instant — lines stamped
   *  later in that minute were previously excluded, so a range
   *  ending "now" (to the minute) silently dropped the newest
   *  lines (code-review 5.5). */
  const untilMsOf = (v: string | undefined): number | undefined => {
    const ms = msOf(v);
    return ms === undefined ? undefined : ms + 59_999;
  };

  const msg = (text: string, isErr = false): void => {
    const m = root.querySelector("#logMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  /* ---- Date/time picker for the From/To inputs (hand-rolled) ----
   * The inputs stay native datetime-local: fully typeable, with the
   * same value format and change semantics. The picker only composes
   * a value, writes it into the input, and dispatches the input's
   * change event, so applying from the picker is indistinguishable
   * from a manual edit (including the page-1 reset). The popup is
   * fixed-positioned from the anchor's rect so the Logs card cannot
   * clip it; it closes on Apply/Clear, Escape, and outside click. */
  const fromCalBtn = root.querySelector("#logFromCal") as HTMLButtonElement | null;
  const toCalBtn = root.querySelector("#logToCal") as HTMLButtonElement | null;
  const pad2 = (n: number): string => String(n).padStart(2, "0");
  let pickerEl: HTMLDivElement | null = null;
  let pickerInput: HTMLInputElement | null = null;
  let pickerAnchor: HTMLElement | null = null;
  let pickerView = { y: 0, m: 0 };
  let pickerSel = { y: 0, m: 0, d: 0 };
  let pickerTimeState = { hh: 0, mm: 0 };

  const closePicker = (): void => {
    pickerEl?.remove();
    pickerEl = null;
    pickerInput = null;
    pickerAnchor = null;
    document.removeEventListener("mousedown", onPickerOutside, true);
    document.removeEventListener("keydown", onPickerKey, true);
  };
  const onPickerOutside = (ev: MouseEvent): void => {
    const t = ev.target as Node;
    if (pickerEl?.contains(t) || pickerAnchor?.contains(t)) return;
    closePicker();
  };
  const onPickerKey = (ev: KeyboardEvent): void => {
    if (ev.key === "Escape") closePicker();
  };

  /** Read the popup's time fields into pickerTimeState (clamped);
   *  called before any re-render so typed time is never lost. */
  const syncPickerTime = (): void => {
    const h = pickerEl?.querySelector("#dtHour") as HTMLInputElement | null;
    const m = pickerEl?.querySelector("#dtMinute") as HTMLInputElement | null;
    if (!h || !m) return;
    const clamp = (v: string, max: number): number => {
      const n = Number.parseInt(v, 10);
      return Number.isNaN(n) ? 0 : Math.min(max, Math.max(0, n));
    };
    pickerTimeState = { hh: clamp(h.value, 23), mm: clamp(m.value, 59) };
  };

  const renderPicker = (): void => {
    if (!pickerEl) return;
    const { y, m } = pickerView;
    const monthName = new Date(y, m, 1).toLocaleDateString(undefined, { month: "long", year: "numeric" });
    const startDay = new Date(y, m, 1).getDay();
    const daysInMonth = new Date(y, m + 1, 0).getDate();
    const daysInPrev = new Date(y, m, 0).getDate();
    const now = new Date();
    let cells = "";
    for (let i = 0; i < 42; i++) {
      const dn = i - startDay + 1;
      let cy = y; let cm = m; let cd = dn; let other = false;
      if (dn < 1) {
        const pm = new Date(y, m - 1, 1);
        cy = pm.getFullYear(); cm = pm.getMonth(); cd = dn + daysInPrev; other = true;
      } else if (dn > daysInMonth) {
        const nm = new Date(y, m + 1, 1);
        cy = nm.getFullYear(); cm = nm.getMonth(); cd = dn - daysInMonth; other = true;
      }
      const cls = `dt-day${other ? " other" : ""}${cy === pickerSel.y && cm === pickerSel.m && cd === pickerSel.d ? " sel" : ""}${cy === now.getFullYear() && cm === now.getMonth() && cd === now.getDate() ? " today" : ""}`;
      cells += `<button type="button" class="${cls}" data-y="${cy}" data-m="${cm}" data-d="${cd}">${cd}</button>`;
    }
    pickerEl.innerHTML = `
      <div class="dt-head">
        <button type="button" class="btn dt-nav" data-nav="-1" title="Previous month" aria-label="Previous month">${icon("chevL")}</button>
        <span>${esc(monthName)}</span>
        <button type="button" class="btn dt-nav" data-nav="1" title="Next month" aria-label="Next month">${icon("chevR")}</button>
      </div>
      <div class="dt-grid">${["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"].map((w) => `<span class="dt-wd">${w}</span>`).join("")}${cells}</div>
      <div class="dt-time">Time
        <input id="dtHour" type="number" min="0" max="23" value="${pad2(pickerTimeState.hh)}" style="${SELECT_STYLE};width:58px"/> :
        <input id="dtMinute" type="number" min="0" max="59" value="${pad2(pickerTimeState.mm)}" style="${SELECT_STYLE};width:58px"/>
      </div>
      <div class="dt-actions">
        <button type="button" class="btn icon-only" data-act="clear" title="Clear" aria-label="Clear">${icon("x")}</button>
        <button type="button" class="btn" data-act="apply">${icon("check")} Apply</button>
      </div>`;
  };

  const onPickerClick = (ev: Event): void => {
    const t = ev.target as HTMLElement;
    const nav = t.closest("[data-nav]") as HTMLElement | null;
    if (nav) {
      syncPickerTime();
      const nm = new Date(pickerView.y, pickerView.m + Number(nav.getAttribute("data-nav")), 1);
      pickerView = { y: nm.getFullYear(), m: nm.getMonth() };
      renderPicker();
      return;
    }
    const day = t.closest(".dt-day") as HTMLElement | null;
    if (day) {
      syncPickerTime();
      pickerSel = { y: Number(day.dataset.y), m: Number(day.dataset.m), d: Number(day.dataset.d) };
      pickerView = { y: pickerSel.y, m: pickerSel.m };
      renderPicker();
      return;
    }
    const act = t.closest("[data-act]") as HTMLElement | null;
    if (!act || !pickerInput) return;
    if (act.getAttribute("data-act") === "clear") {
      pickerInput.value = "";
      pickerInput.dispatchEvent(new Event("change", { bubbles: true }));
      closePicker();
      return;
    }
    syncPickerTime();
    pickerInput.value = `${pickerSel.y}-${pad2(pickerSel.m + 1)}-${pad2(pickerSel.d)}T${pad2(pickerTimeState.hh)}:${pad2(pickerTimeState.mm)}`;
    pickerInput.dispatchEvent(new Event("change", { bubbles: true }));
    closePicker();
  };

  const openPicker = (input: HTMLInputElement, anchor: HTMLElement): void => {
    if (pickerEl && pickerInput === input) { closePicker(); return; }
    closePicker();
    const parsed = input.value ? new Date(input.value) : null;
    const base = parsed && !Number.isNaN(parsed.getTime()) ? parsed : new Date();
    pickerSel = { y: base.getFullYear(), m: base.getMonth(), d: base.getDate() };
    pickerView = { y: pickerSel.y, m: pickerSel.m };
    pickerTimeState = { hh: base.getHours(), mm: base.getMinutes() };
    pickerInput = input;
    pickerAnchor = anchor;
    pickerEl = document.createElement("div");
    pickerEl.className = "dt-pop";
    pickerEl.style.visibility = "hidden";
    pickerEl.addEventListener("click", onPickerClick);
    document.body.appendChild(pickerEl);
    renderPicker();
    const r = anchor.getBoundingClientRect();
    const w = pickerEl.offsetWidth;
    const h = pickerEl.offsetHeight;
    let top = r.bottom + 4;
    if (top + h > window.innerHeight - 8) top = Math.max(8, r.top - h - 4);
    pickerEl.style.top = `${top}px`;
    pickerEl.style.left = `${Math.min(Math.max(8, r.left), Math.max(8, window.innerWidth - w - 8))}px`;
    pickerEl.style.visibility = "";
    document.addEventListener("mousedown", onPickerOutside, true);
    document.addEventListener("keydown", onPickerKey, true);
  };

  const onFromCal = (): void => { if (fromInput && fromCalBtn) openPicker(fromInput, fromCalBtn); };
  const onToCal = (): void => { if (toInput && toCalBtn) openPicker(toInput, toCalBtn); };
  fromCalBtn?.addEventListener("click", onFromCal);
  toCalBtn?.addEventListener("click", onToCal);

  /** Merge scope options: "all", backend ids, and the payload's scope
   *  list (scopes with a previous-run ring labeled as such). Existing
   *  options get their labels refreshed — a backend that gains an
   *  archived ring mid-session must gain the suffix, not keep the bare
   *  label from its first insert. */
  const mergeScopeOptions = (entries: { value: string; label: string }[]): void => {
    if (!scopeSel) return;
    const byValue = new Map(Array.from(scopeSel.options).map((o) => [o.value, o]));
    for (const e of entries) {
      const existing = byValue.get(e.value);
      if (existing) {
        if (existing.textContent !== e.label) existing.textContent = e.label;
        continue;
      }
      scopeSel.insertAdjacentHTML("beforeend", `<option value="${esc(e.value)}">${esc(e.label)}</option>`);
      byValue.set(e.value, scopeSel.options[scopeSel.options.length - 1]);
    }
  };

  /* ---- Session state + recent searches (tracker #59) ---- */
  const recentsEl = root.querySelector("#logRecents") as HTMLElement | null;
  /** Set once this wire has applied its entry state. The router can
   *  call a superseded wire's cleanup after its DOM was replaced;
   *  such a wire must not snapshot its never-applied defaults over
   *  the session state. */
  let stateApplied = false;

  /** Mirror the controls (and the closure page) into the
   *  module-level session state. */
  const syncLogsState = (): void => {
    logsViewState = {
      scope: scopeSel?.value ?? "all",
      level: levelSel?.value ?? "",
      query: searchInput?.value ?? "",
      from: fromInput?.value ?? "",
      to: toInput?.value ?? "",
      sort: sortSel?.value === "asc" ? "asc" : "desc",
      limit: Number(limitSel?.value ?? 200),
      follow: followChk?.checked ?? false,
      page,
    };
  };

  /** The currently applied query as a search set, stamped now. The
   *  query is the trimmed value — the one load() actually sends —
   *  so sets differing only in stray spaces dedupe together. */
  const currentSearchSet = (): LogSearchSet => ({
    scope: logsViewState.scope,
    level: logsViewState.level,
    query: logsViewState.query.trim(),
    from: logsViewState.from,
    to: logsViewState.to,
    sort: logsViewState.sort,
    limit: logsViewState.limit,
    ranAt: new Date().toISOString(),
  });

  /** Recent searches: an expandable section under the filter card,
   *  collapsed by default (principle 1), omitted entirely while
   *  the history is empty (principle 13). An entry click applies
   *  the whole set; Clear empties the history (words + icon —
   *  clearing is destructive, principle 15). */
  const paintRecents = (): void => {
    if (!recentsEl) return;
    if (!logsRecents.length) { recentsEl.innerHTML = ""; return; }
    const open = logsRecentsOpen;
    recentsEl.innerHTML = `<div class="card" style="margin-bottom:12px">
      <div style="display:flex;align-items:center;gap:8px">
        <button class="btn" id="logRecentsToggle" aria-expanded="${open}">${icon(open ? "chevD" : "chevR")} Recent searches (${logsRecents.length})</button>
        <span class="spacer" style="flex:1"></span>
        ${open ? `<button class="btn" id="logRecentsClear">${icon("x")} Clear</button>` : ""}
      </div>
      ${open ? `<div style="display:grid;gap:2px;margin-top:8px">${logsRecents.map((e, i) => `<button class="btn" data-recent="${i}" title="${esc(fmtDateTime(e.ranAt))}" style="justify-content:flex-start;text-align:left"><span style="flex:1">${esc(logSearchSummary(e))}</span><span style="color:var(--text3);font-size:11px">${esc(fmtRanAt(e.ranAt))}</span></button>`).join("")}</div>` : ""}
    </div>`;
    recentsEl.querySelector("#logRecentsToggle")?.addEventListener("click", () => {
      logsRecentsOpen = !logsRecentsOpen;
      paintRecents();
    });
    recentsEl.querySelector("#logRecentsClear")?.addEventListener("click", () => {
      logsRecents = [];
      saveLogsRecents();
      paintRecents();
    });
    recentsEl.querySelectorAll("[data-recent]").forEach((b) => {
      b.addEventListener("click", () => {
        const e = logsRecents[Number((b as HTMLElement).dataset.recent)];
        if (e) applySearchSet(e);
      });
    });
  };

  /** Capture the applied set into the recents and repaint them. */
  const captureSearch = (): void => {
    recordLogSearch(currentSearchSet());
    paintRecents();
  };

  /** Write a search set into the session state and the controls,
   *  restart paging at 1, and run it. Follow is not part of a set
   *  and is left as it stands. */
  const applySearchSet = (s: LogSearchSet): void => {
    mergeScopeOptions([{ value: s.scope, label: s.scope }]);
    if (scopeSel) scopeSel.value = s.scope;
    if (levelSel) levelSel.value = s.level;
    if (searchInput) searchInput.value = s.query;
    if (fromInput) fromInput.value = s.from;
    if (toInput) toInput.value = s.to;
    if (sortSel) sortSel.value = s.sort;
    if (limitSel) limitSel.value = String(s.limit);
    page = 1;
    syncLogsState();
    captureSearch();
    void load();
  };

  const filtersActive = (): boolean => Boolean(
    levelSel?.value || searchInput?.value.trim() || fromInput?.value || toInput?.value,
  );

  const load = async (): Promise<void> => {
    const seq = ++loadSeq;
    const stale = (): boolean => disposed || seq !== loadSeq;
    try {
      const scope = scopeSel?.value ?? "all";
      const limit = Number(limitSel?.value ?? 200);
      const level = levelSel?.value || undefined;
      const query = searchInput?.value.trim() || undefined;
      const fetchPage = () => getLogs(scope, {
        limit,
        level,
        query,
        sinceMs: msOf(fromInput?.value),
        untilMs: untilMsOf(toInput?.value),
        order: sortSel?.value === "asc" ? "asc" : "desc",
        offset: (page - 1) * limit,
      });
      let r = await fetchPage();
      if (stale()) return;
      // The filtered set may have shrunk below the current page
      // (e.g. a Follow refresh after lines aged out): clamp to the
      // last page and refetch, so the page shown is real. The
      // clamp LOOPS (bounded): the set can shrink again between
      // the two fetches, and a single clamp-and-refetch could
      // still land past the end (code-review 5.6). Page targets
      // the CURRENT page value: a page turn during a fetch
      // supersedes this load anyway (the seq check).
      for (let attempt = 0; attempt < 3; attempt += 1) {
        const lastPage = Math.max(1, Math.ceil(r.data.total / limit));
        if (page <= lastPage) break;
        page = lastPage;
        r = await fetchPage();
        if (stale()) return;
      }
      // The clamp may have moved the page; the session state keeps
      // the page that is actually shown (tracker #59).
      logsViewState.page = page;
      setBanner(root, "logBanner", bannerHtml(r));
      mergeScopeOptions(r.data.scopes.map((s) => ({
        value: s.id,
        label: s.previous ? `${s.id} (incl. previous run)` : s.id,
      })));
      const pages = Math.max(1, Math.ceil(r.data.total / limit));
      // Pagination values as Label: Value, the total with thousands
      // separators, and position as a micro-bar (fill = page/pages).
      if (pageInfo) pageInfo.textContent = `Page: ${page} of ${pages}`;
      if (matchInfo) matchInfo.textContent = `Matching: ${fmtCount(r.data.total)}`;
      if (posBar) posBar.innerHTML = meter((page / pages) * 100, 64, 6, C.blue);
      if (prevBtn) prevBtn.disabled = page <= 1;
      if (nextBtn) nextBtn.disabled = page >= pages;
      const b = root.querySelector("#logBody");
      if (!b) return;
      if (r.error) { b.innerHTML = emptyHtml(`Logs unavailable: ${r.error}`); return; }
      if (!r.data.lines.length) {
        b.innerHTML = emptyHtml(filtersActive() || r.data.total_matching > 0
          ? "No matching lines."
          : "No lines yet.");
        return;
      }
      b.innerHTML = `<div class="logstream">${r.data.lines.map((row) => logRow(row, scope === "all")).join("")}</div>`;
    } catch {
      // getLogs wraps invoke errors into the result (r.error is
      // rendered above); a throw here is a programming error — the
      // next load (filter change, Follow tick) repaints.
    }
  };

  // Scope options: "all" plus every backend id (configured or not — a
  // retained ring can outlive its config entry).
  const backs = await getBackends();
  if (!disposed && backs.live && !backs.error) {
    mergeScopeOptions(backs.data.map((bk) => ({ value: bk.id, label: bk.id })));
  }
  // Entry state (tracker #59). A parametrized entry — a deep link
  // (#/logs?scope=<id>&follow=1, the entity cards' "go to logs"
  // buttons; a scope alone from the wizard's failure link) — applies
  // its params over the fresh defaults exactly as it did before #59,
  // and the applied result becomes the session state and is
  // captured as a recent search. A plain entry instead restores
  // the session state verbatim — scope, level, search text, time
  // bounds, sort, limit, Follow, and page.
  const paramEntry = initialScope !== undefined || initialFollow;
  if (paramEntry) {
    // Deep link (#/logs?scope=<id>): select it, adding the option
    // when it is not a known backend id (e.g. a previous-run scope).
    if (scopeSel && initialScope) {
      mergeScopeOptions([{ value: initialScope, label: initialScope }]);
      scopeSel.value = initialScope;
    }
    if (followChk) followChk.checked = initialFollow;
    stateApplied = true;
    syncLogsState();
    captureSearch();
  } else {
    // The retained scope may need its option added first (a scope
    // whose backend has left the config, or a previous-run scope
    // the first payload has not listed yet).
    mergeScopeOptions([{ value: logsViewState.scope, label: logsViewState.scope }]);
    if (scopeSel) scopeSel.value = logsViewState.scope;
    if (levelSel) levelSel.value = logsViewState.level;
    if (searchInput) searchInput.value = logsViewState.query;
    if (fromInput) fromInput.value = logsViewState.from;
    if (toInput) toInput.value = logsViewState.to;
    if (sortSel) sortSel.value = logsViewState.sort;
    if (limitSel) limitSel.value = String(logsViewState.limit);
    if (followChk) followChk.checked = logsViewState.follow;
    page = logsViewState.page;
    stateApplied = true;
  }
  paintRecents();

  /** Any filter change (scope, level, text, time bounds, sort, page
   *  size) restarts paging at page 1 — and is a search being run:
   *  it updates the session state and is captured into the recents
   *  (tracker #59). Refresh, pager turns, and Follow toggles only
   *  re-run or navigate the current search, so they sync the
   *  session state without capturing. */
  const onFilterChange = (): void => { page = 1; syncLogsState(); captureSearch(); void load(); };
  /** Manual Refresh reloads the current page as-is. */
  const onRefresh = (): void => { void load(); };
  const onSearch = (): void => {
    if (searchTimer !== null) window.clearTimeout(searchTimer);
    searchTimer = window.setTimeout(() => {
      searchTimer = null;
      page = 1;
      syncLogsState();
      captureSearch();
      void load();
    }, 300);
  };
  const onPrev = (): void => { if (page > 1) { page -= 1; syncLogsState(); void load(); } };
  const onNext = (): void => { page += 1; syncLogsState(); void load(); };
  const onFollow = (): void => {
    syncLogsState();
    if (followTimer !== null) { window.clearInterval(followTimer); followTimer = null; }
    if (followChk?.checked) {
      followTimer = window.setInterval(() => { if (!disposed) void load(); }, 2000);
      void load();
    }
  };
  const openFolderBtn = root.querySelector("#logOpenFolder") as HTMLButtonElement | null;
  let lastExportPath = "";
  const onExport = async (): Promise<void> => {
    if (exportBtn) exportBtn.disabled = true;
    const r = await exportDiagnostics();
    if (exportBtn) exportBtn.disabled = false;
    if (disposed) return;
    if (r.error) { msg(`Export failed: ${r.error}`, true); return; }
    msg(r.data.path ? `Diagnostics bundle written to ${r.data.path}.` : "Diagnostics bundle written.");
    if (r.data.path && openFolderBtn) {
      lastExportPath = r.data.path;
      openFolderBtn.style.display = "";
    }
  };
  openFolderBtn?.addEventListener("click", () => { if (lastExportPath) void openFolder(lastExportPath); });
  scopeSel?.addEventListener("change", onFilterChange);
  levelSel?.addEventListener("change", onFilterChange);
  fromInput?.addEventListener("change", onFilterChange);
  toInput?.addEventListener("change", onFilterChange);
  sortSel?.addEventListener("change", onFilterChange);
  limitSel?.addEventListener("change", onFilterChange);
  searchInput?.addEventListener("input", onSearch);
  followChk?.addEventListener("change", onFollow);
  refreshBtn?.addEventListener("click", onRefresh);
  prevBtn?.addEventListener("click", onPrev);
  nextBtn?.addEventListener("click", onNext);
  exportBtn?.addEventListener("click", onExport);

  // Arrival with Follow on — a deep link's follow=1, or a session
  // state that was left following (tracker #59): arrive in exactly
  // the state the Follow toggle produces — checkbox on, 2 s
  // re-poll running, first load fired by onFollow itself, so the
  // restored filters' results load before the tail resumes.
  if (followChk?.checked) {
    onFollow();
  } else {
    await load();
  }

  return () => {
    disposed = true;
    // Final session-state snapshot: the controls still hold the
    // operator's last values (the router runs cleanup before the
    // next view's markup replaces this DOM) — including search
    // text whose debounce had not fired yet. Guarded by
    // stateApplied so a superseded wire that never applied its
    // entry state cannot overwrite the session with defaults.
    if (stateApplied) syncLogsState();
    closePicker();
    if (followTimer !== null) window.clearInterval(followTimer);
    if (searchTimer !== null) window.clearTimeout(searchTimer);
    scopeSel?.removeEventListener("change", onFilterChange);
    levelSel?.removeEventListener("change", onFilterChange);
    fromInput?.removeEventListener("change", onFilterChange);
    toInput?.removeEventListener("change", onFilterChange);
    fromCalBtn?.removeEventListener("click", onFromCal);
    toCalBtn?.removeEventListener("click", onToCal);
    sortSel?.removeEventListener("change", onFilterChange);
    limitSel?.removeEventListener("change", onFilterChange);
    searchInput?.removeEventListener("input", onSearch);
    followChk?.removeEventListener("change", onFollow);
    refreshBtn?.removeEventListener("click", onRefresh);
    prevBtn?.removeEventListener("click", onPrev);
    nextBtn?.removeEventListener("click", onNext);
    exportBtn?.removeEventListener("click", onExport);
  };
}

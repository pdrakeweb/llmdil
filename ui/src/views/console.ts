/* Debug console (docs/debug-console.md).
   One pane: an output scrollback with a line prompt for built-in
   commands (the same grammar agents use over the admin API), and a
   side panel with Import (paste → reviewed recipe), Recipes, History
   (results tables) and Settings (the security gates and executable
   allowlist). The raw shell is a separate mode, off by default,
   enabled per session with the toggle; the prompt then talks to a
   pwsh session instead of the built-ins. Nothing pasted ever runs
   directly: Import produces a recipe to review and edit first. */

import {
  consoleExec, consoleImport, consolePlan, consoleRecipeDelete, consoleRecipeGet, consoleRecipeSave,
  consoleRecipesList, consoleRunCancel, consoleRunExport, consoleRunOutput, consoleRunResult, consoleRunStart,
  consoleRunsList, consoleSettingsGet, consoleSettingsSet, consoleShellClose, consoleShellOpen, consoleShellRead,
  consoleShellSend, isTauri,
} from "../api";
import type { ConsoleOutputLine, Json } from "../api";
import { esc } from "../html";
import { icon } from "../icons";

export function renderConsole(): string {
  return `<div class="topbar"><h1>Console</h1><span class="spacer"></span>
    <label class="con-shell-toggle" title="Raw shell: a pwsh session for this console only. Off by default; every line is audited.">
      <input type="checkbox" id="conShell"> Raw shell</label>
  </div>
  <div class="view con-view"><div id="conRoot" class="con-root"></div></div>`;
}

type Tab = "import" | "recipes" | "history" | "settings";

interface OutLine { stream: string; text: string }

const MAX_LINES = 5000;

function streamClass(s: string): string {
  switch (s) {
    case "stderr": return "con-err";
    case "console": return "con-sys";
    case "input": return "con-in";
    case "result": return "con-res";
    default: return "";
  }
}

function newSessionId(): string {
  return `ui-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

function fmtTs(v: unknown): string {
  if (typeof v !== "number") return "—";
  return new Date(v).toLocaleString(undefined, { dateStyle: "short", timeStyle: "medium" });
}

/** The results table from a run summary's rows. */
function resultsTable(summary: Json | undefined): string {
  const rows = (summary?.rows as Json[] | undefined) ?? [];
  if (!rows.length) return `<div class="con-muted">No results.</div>`;
  const body = rows.map((r) => {
    const pp = r.pp_ts_per_depth as Record<string, number> | undefined;
    const ppText = pp && Object.keys(pp).length
      ? Object.entries(pp).map(([d, v]) => `d${d}: ${v >= 100 ? v.toFixed(0) : v.toFixed(1)}`).join(", ") : "—";
    const verdict = String(r.verdict ?? "");
    const cls = verdict === "ok" ? "badge-ok" : verdict === "expected_failure" ? "badge-warn" : "badge-err";
    return `<tr><td>${esc(r.variant)}</td><td class="${cls}">${esc(r.exit)}</td><td>${esc(r.decoded || "—")}</td>
      <td class="mono">${esc(ppText)}</td><td>${esc(r.failing_depth ?? "—")}</td><td class="mono">${esc(r.cuda_error || "—")}</td></tr>`;
  }).join("");
  return `<div style="overflow-x:auto"><table class="tbl con-table"><thead><tr><th>Variant</th><th>Exit</th><th>Decoded</th>
    <th>pp t/s per depth</th><th>Failing depth</th><th>CUDA error excerpt</th></tr></thead><tbody>${body}</tbody></table></div>`;
}

export async function wireConsole(root: HTMLElement): Promise<() => void> {
  const host = root.querySelector<HTMLElement>("#conRoot");
  const shellToggle = root.querySelector<HTMLInputElement>("#conShell");
  if (!host || !shellToggle) return () => {};
  const hostEl: HTMLElement = host;
  const toggle: HTMLInputElement = shellToggle;

  let disposed = false;
  const sessionId = newSessionId();
  const lines: OutLine[] = [];
  const history: string[] = [];
  let histPos = -1;
  let tab: Tab = "import";
  let busy = false;

  // Active run following.
  let runId: string | null = null;
  let runSince = 0;
  let runTimer: number | null = null;

  // Raw shell.
  let shellOn = false;
  let shellSince = 0;
  let shellTimer: number | null = null;

  // Import / editor state.
  let pasteText = "";
  let pasteName = "";
  let fold = true;
  let importResult: Json | null = null;
  let editorText = "";
  let editorErr = "";
  let planView: Json | null = null;
  let paramsText = "";
  let variantText = "";
  let confirmFor: { ids: string[]; reason: string } | null = null;

  let recipes: Json[] = [];
  let runs: Json[] = [];
  let shownRun: Json | null = null;
  let settings: Json | null = null;
  let settingsMsg = "";

  hostEl.innerHTML = `<div class="con-main">
      <div class="con-out" id="conOut" tabindex="0"></div>
      <div class="con-prompt"><span class="con-ps" id="conPs">llmdil&gt;</span>
        <input id="conLine" spellcheck="false" autocomplete="off" placeholder="help · recipes · run &lt;recipe&gt; · logs &lt;backend&gt; --prev · gpu · gguf &lt;model&gt;">
        <button class="btn" id="conCancel" title="Cancel the active run" disabled>${icon("ban")} Cancel run</button></div>
    </div>
    <div class="con-side">
      <div class="con-tabs" id="conTabs"></div>
      <div class="con-panel" id="conPanel"></div>
    </div>`;
  const outEl = hostEl.querySelector<HTMLElement>("#conOut")!;
  const lineEl = hostEl.querySelector<HTMLInputElement>("#conLine")!;
  const psEl = hostEl.querySelector<HTMLElement>("#conPs")!;
  const cancelBtn = hostEl.querySelector<HTMLButtonElement>("#conCancel")!;
  const tabsEl = hostEl.querySelector<HTMLElement>("#conTabs")!;
  const panelEl = hostEl.querySelector<HTMLElement>("#conPanel")!;

  function append(stream: string, text: string): void {
    const atBottom = outEl.scrollTop + outEl.clientHeight >= outEl.scrollHeight - 24;
    const frag = document.createDocumentFragment();
    for (const t of text.split("\n")) {
      lines.push({ stream, text: t });
      const div = document.createElement("div");
      div.className = `con-line ${streamClass(stream)}`;
      div.textContent = t;
      frag.appendChild(div);
    }
    outEl.appendChild(frag);
    while (lines.length > MAX_LINES) {
      lines.shift();
      outEl.firstChild?.remove();
    }
    if (atBottom) outEl.scrollTop = outEl.scrollHeight;
  }

  function appendHtml(html: string): void {
    const div = document.createElement("div");
    div.className = "con-block";
    div.innerHTML = html;
    outEl.appendChild(div);
    outEl.scrollTop = outEl.scrollHeight;
  }

  /* ---------------- Runs ---------------- */

  function followRun(id: string): void {
    runId = id;
    runSince = 0;
    cancelBtn.disabled = false;
    if (runTimer !== null) window.clearInterval(runTimer);
    runTimer = window.setInterval(() => void pollRun(), 500);
    void pollRun();
  }

  let polling = false;
  async function pollRun(): Promise<void> {
    if (!runId || polling || disposed) return;
    polling = true;
    try {
      const id = runId;
      const r = await consoleRunOutput(id, runSince);
      if (r.error) { append("stderr", `output poll failed: ${r.error}`); return; }
      for (const l of r.data.lines as ConsoleOutputLine[]) append(l.stream, l.line);
      runSince = r.data.next;
      if (r.data.done && !r.data.more) {
        if (runTimer !== null) window.clearInterval(runTimer);
        runTimer = null;
        runId = null;
        cancelBtn.disabled = true;
        const res = await consoleRunResult(id);
        if (!res.error) {
          appendHtml(`<div class="con-res-head">${icon("check")} ${esc(id)} — ${esc(res.data.status)} · ${esc((res.data.summary as Json | undefined)?.headline ?? "")}</div>${resultsTable(res.data.summary as Json | undefined)}`);
        }
        if (tab === "history") await loadRuns();
      }
    } finally {
      polling = false;
    }
  }

  async function startRun(doc: Json, confirm: string[]): Promise<void> {
    const params = parseParams(paramsText);
    const variant = variantText.trim() ? Number(variantText.trim()) : null;
    const r = await consoleRunStart(doc, params, Number.isFinite(variant as number) ? variant : null, confirm);
    if (r.error) { append("stderr", `run failed to start: ${r.error}`); return; }
    const d = r.data;
    if (d.started === true) {
      confirmFor = null;
      append("console", `[console] run ${String(d.run_id)} started`);
      followRun(String(d.run_id));
    } else if (d.needs_confirmation === true) {
      confirmFor = { ids: (d.confirm_stop as string[]) ?? [], reason: String(d.reason ?? "") };
      planView = (d.plan as Json) ?? planView;
      append("console", `[console] ${String(d.reason ?? "confirmation needed")}`);
    } else {
      planView = (d.plan as Json) ?? planView;
      append("stderr", `not started: ${String(d.reason ?? "")} ${((d.errors as string[]) ?? []).join("; ")}`);
    }
    paint();
  }

  function parseParams(t: string): Record<string, string> {
    const out: Record<string, string> = {};
    for (const part of t.split(/\n|;/)) {
      const i = part.indexOf("=");
      if (i > 0) out[part.slice(0, i).trim()] = part.slice(i + 1).trim();
    }
    return out;
  }

  function editorDoc(): Json | null {
    try {
      const v = JSON.parse(editorText) as Json;
      editorErr = "";
      return v;
    } catch (e) {
      editorErr = `The recipe JSON does not parse: ${e instanceof Error ? e.message : String(e)}`;
      return null;
    }
  }

  /* ---------------- Prompt ---------------- */

  async function submit(line: string): Promise<void> {
    const t = line.trim();
    if (!t) return;
    history.unshift(t);
    histPos = -1;
    if (shellOn) {
      const r = await consoleShellSend(sessionId, t);
      if (r.error) append("stderr", `shell: ${r.error}`);
      return;
    }
    append("input", `llmdil> ${t}`);
    if (t === "import" || t === "paste") { tab = "import"; paint(); return; }
    if (t === "clear" || t === "cls") { lines.length = 0; outEl.innerHTML = ""; return; }
    busy = true;
    try {
      const r = await consoleExec(t, sessionId);
      if (r.error) { append("stderr", r.error); return; }
      const d = r.data;
      if (d.text) append(d.ok ? "result" : "stderr", d.text.replace(/\n$/, ""));
      const data = d.data as Json | undefined;
      if (data && data.started === true && typeof data.run_id === "string") followRun(data.run_id);
      if (data && data.needs_confirmation === true) {
        append("console", `[console] confirm with: ${t} --stop ${((data.confirm_stop as string[]) ?? []).join(",")}`);
      }
      if (d.command === "show" && data?.summary) appendHtml(resultsTable(data.summary as Json));
    } finally {
      busy = false;
    }
  }

  lineEl.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter" && !busy) {
      const v = lineEl.value;
      lineEl.value = "";
      void submit(v);
    } else if (ev.key === "ArrowUp") {
      if (history.length) { histPos = Math.min(histPos + 1, history.length - 1); lineEl.value = history[histPos]; }
      ev.preventDefault();
    } else if (ev.key === "ArrowDown") {
      histPos = Math.max(histPos - 1, -1);
      lineEl.value = histPos >= 0 ? history[histPos] : "";
      ev.preventDefault();
    }
  });
  cancelBtn.addEventListener("click", () => {
    if (runId) void consoleRunCancel(runId).then((r) => { if (r.error) append("stderr", r.error); });
  });

  /* ---------------- Raw shell ---------------- */

  async function setShell(on: boolean): Promise<void> {
    if (on) {
      const r = await consoleShellOpen(sessionId);
      if (r.error) { append("stderr", `raw shell: ${r.error}`); toggle.checked = false; return; }
      shellOn = true;
      shellSince = 0;
      psEl.textContent = "pwsh>";
      psEl.classList.add("con-ps-shell");
      append("console", "[console] raw shell ON for this console session — lines go to pwsh, not the built-ins. Turn it off to return.");
      shellTimer = window.setInterval(() => void pollShell(), 400);
    } else {
      shellOn = false;
      if (shellTimer !== null) window.clearInterval(shellTimer);
      shellTimer = null;
      psEl.textContent = "llmdil>";
      psEl.classList.remove("con-ps-shell");
      await consoleShellClose(sessionId);
      append("console", "[console] raw shell OFF");
    }
  }

  async function pollShell(): Promise<void> {
    if (!shellOn) return;
    const r = await consoleShellRead(sessionId, shellSince);
    if (r.error) return;
    for (const l of (r.data.lines as { seq: number; stream: string; line: string }[]) ?? []) {
      append(l.stream === "input" ? "input" : l.stream, l.stream === "input" ? `pwsh> ${l.line}` : l.line);
      shellSince = l.seq;
    }
    if (r.data.alive === false) { toggle.checked = false; await setShell(false); }
  }

  toggle.addEventListener("change", () => void setShell(toggle.checked));

  /* ---------------- Side panel ---------------- */

  async function loadRecipes(): Promise<void> {
    const r = await consoleRecipesList();
    recipes = (r.data.recipes as Json[]) ?? [];
    paint();
  }
  async function loadRuns(): Promise<void> {
    const r = await consoleRunsList();
    runs = (r.data.runs as Json[]) ?? [];
    paint();
  }
  async function loadSettings(): Promise<void> {
    const r = await consoleSettingsGet();
    settings = r.error ? null : r.data;
    paint();
  }

  function planHtml(p: Json | null): string {
    if (!p) return "";
    const steps = (p.steps as Json[]) ?? [];
    const stops = (p.stop_backends as Json[]) ?? [];
    const errs = (p.errors as string[]) ?? [];
    const warns = (p.warnings as string[]) ?? [];
    return `<div class="con-plan">
      <div class="con-plan-sum">${esc(p.summary ?? "")}</div>
      ${steps.map((s) => `<div class="con-step"><b>[${Number(s.index) + 1}] ${esc(s.label)}</b>
        <div class="mono con-cmd">${esc(s.command_line)}</div>
        ${Object.keys((s.env as Json) ?? {}).length ? `<div class="mono con-env">env: ${esc(Object.entries(s.env as Json).map(([k, v]) => `${k}=${String(v)}`).join(" "))}</div>` : ""}
        ${s.description ? `<div class="con-muted">${esc(s.description)}</div>` : ""}</div>`).join("")}
      ${stops.length ? `<div class="con-stops">${icon("warn")} Stops (drained first, restored afterwards): ${stops.map((s) => `<b>${esc(s.id)}</b> <span class="con-muted">(${esc(s.reason)})</span>`).join(", ")}</div>` : ""}
      ${warns.map((w) => `<div class="badge-warn">${esc(w)}</div>`).join("")}
      ${errs.map((e) => `<div class="badge-err">${icon("err")} ${esc(e)}</div>`).join("")}
    </div>`;
  }

  function panelImport(): string {
    const ir = importResult;
    const folds = (ir?.folds as Json[]) ?? [];
    const warnings = (ir?.warnings as string[]) ?? [];
    return `<div class="con-sec">
      <div class="con-label">Paste a block of diagnostic commands (PowerShell or cmd)</div>
      <textarea id="conPaste" class="con-ta" rows="8" spellcheck="false" placeholder="$env:CUDA_LAUNCH_BLOCKING=1&#10;$m=&quot;...gguf&quot;&#10;.\\llama-bench.exe -m $m -ngl 99 -d 0,4096,8192 -fa 1   # expect crash">${esc(pasteText)}</textarea>
      <div class="con-row"><input id="conPasteName" class="con-in" placeholder="recipe name (optional)" value="${esc(pasteName)}">
        <label><input type="checkbox" id="conFold"${fold ? " checked" : ""}> Fold same-tool runs into variants</label>
        <button class="btn primary" data-act="parse">${icon("search")} Parse</button></div>
      <div class="con-muted">Parsing never runs anything. Review and edit the recipe below, then plan, save or run it.</div>
    </div>
    ${ir ? `<div class="con-sec">
      ${warnings.map((w) => `<div class="badge-warn">${icon("warn")} ${esc(w)}</div>`).join("")}
      ${folds.map((f) => `<div class="con-muted">${icon("layers")} Folded ${esc(f.folded)} invocations of ${esc(f.exe)} into variants (varying ${esc(((f.varying as string[]) ?? []).join(", "))}). <a href="#" data-act="unfold">Unfold</a></div>`).join("")}
    </div>` : ""}
    <div class="con-sec">
      <div class="con-label">Recipe (JSON — edit freely)</div>
      <textarea id="conEditor" class="con-ta mono" rows="16" spellcheck="false">${esc(editorText)}</textarea>
      ${editorErr ? `<div class="badge-err">${esc(editorErr)}</div>` : ""}
      <div class="con-row">
        <input id="conParams" class="con-in" placeholder="params: k=v; k2=v2" value="${esc(paramsText)}">
        <input id="conVariant" class="con-in con-narrow" placeholder="variant #" value="${esc(variantText)}">
      </div>
      <div class="con-row">
        <button class="btn" data-act="plan">${icon("eye")} Plan</button>
        <button class="btn" data-act="save">${icon("check")} Save recipe</button>
        <button class="btn primary" data-act="run">${icon("play")} Run</button>
      </div>
      ${confirmFor ? `<div class="con-confirm">${icon("warn")} ${esc(confirmFor.reason)}
        <div class="con-row"><button class="btn primary" data-act="confirm-run">${icon("play")} Stop ${esc(confirmFor.ids.join(", "))} and run</button>
        <button class="btn" data-act="confirm-cancel">Cancel</button></div></div>` : ""}
      ${planHtml(planView)}
    </div>`;
  }

  function panelRecipes(): string {
    if (!recipes.length) return `<div class="con-muted">No recipes.</div>`;
    return recipes.map((r) => `<div class="con-item">
      <div><b>${esc(r.name)}</b> ${r.builtin ? `<span class="con-tag">built-in</span>` : ""}
        <span class="con-muted">${esc(r.steps)} step(s), ${esc(r.variants)} variant(s)</span></div>
      <div class="con-muted">${esc(r.title || r.description || "")}</div>
      ${r.error ? `<div class="badge-err">${esc(r.error)}</div>` : ""}
      <div class="con-row"><button class="btn" data-act="open-recipe" data-name="${esc(r.name)}">${icon("pencil")} Open</button>
        ${r.builtin ? "" : `<button class="btn danger" data-act="delete-recipe" data-name="${esc(r.name)}">${icon("trash")} Delete</button>`}</div>
    </div>`).join("");
  }

  function panelHistory(): string {
    const list = runs.length ? runs.map((r) => `<div class="con-item">
      <div><b class="mono">${esc(r.id)}</b> <span class="con-tag">${esc(r.status)}</span></div>
      <div class="con-muted">${esc(fmtTs(r.started_ms))} · ${esc((r.caller as Json | undefined)?.actor ?? "")} · ${esc(r.headline ?? "")}</div>
      <div class="con-row"><button class="btn" data-act="show-run" data-id="${esc(r.id)}">${icon("eye")} Results</button>
        <button class="btn" data-act="export-run" data-id="${esc(r.id)}">${icon("download")} Export JSON</button></div>
    </div>`).join("") : `<div class="con-muted">No runs yet.</div>`;
    const shown = shownRun ? `<div class="con-sec"><div class="con-label">${esc(shownRun.id)} — ${esc(shownRun.status)}</div>
      ${resultsTable(shownRun.summary as Json | undefined)}
      ${((shownRun.steps as Json[]) ?? []).filter((s) => s.verdict !== "ok" && s.verdict !== "skipped" && (s.analysis as Json | undefined)?.stderr_excerpt)
        .map((s) => `<div class="con-label">[${Number(s.index) + 1}] ${esc(s.label)} — stderr excerpt</div><pre class="con-pre">${esc((s.analysis as Json).stderr_excerpt)}</pre>`).join("")}
      </div>` : "";
    return shown + list;
  }

  function panelSettings(): string {
    if (!settings) return `<div class="con-muted">Settings unavailable.</div>`;
    const allow = ((settings.exec_allowlist as string[]) ?? []).join("\n");
    return `<div class="con-sec">
      <label class="con-check"><input type="checkbox" id="conRemote"${settings.allow_remote_exec ? " checked" : ""}>
        Allow remote execution — let admin API requests from other machines run recipes, lifecycle commands and set-flags (off: loopback only)</label>
      <label class="con-check"><input type="checkbox" id="conApiShell"${settings.api_raw_shell ? " checked" : ""}>
        Allow the raw shell over the admin API (off: agents get built-ins and allowlisted recipe steps only)</label>
      <div class="con-label">Executable allowlist (one per line: a path with an optional * file glob, or a program name resolved on PATH)</div>
      <textarea id="conAllow" class="con-ta mono" rows="5" spellcheck="false">${esc(allow)}</textarea>
      <div class="con-row"><button class="btn primary" data-act="save-settings">${icon("check")} Save</button>
        <button class="btn" data-act="reset-allow">Reset allowlist</button></div>
      ${settingsMsg ? `<div class="con-muted">${esc(settingsMsg)}</div>` : ""}
      <div class="con-muted">These settings can only be changed here, never over the admin API.</div>
    </div>`;
  }

  function captureInputs(): void {
    const v = (id: string) => panelEl.querySelector<HTMLInputElement | HTMLTextAreaElement>(`#${id}`)?.value;
    pasteText = v("conPaste") ?? pasteText;
    pasteName = v("conPasteName") ?? pasteName;
    editorText = v("conEditor") ?? editorText;
    paramsText = v("conParams") ?? paramsText;
    variantText = v("conVariant") ?? variantText;
    const f = panelEl.querySelector<HTMLInputElement>("#conFold");
    if (f) fold = f.checked;
  }

  // `capture: false` when the caller just set the editor state itself
  // (parse, unfold, open recipe): reading the DOM back first would
  // overwrite it with the previous, often empty, textarea contents.
  function paint(capture = true): void {
    if (disposed) return;
    if (capture) captureInputs();
    const tabs: [Tab, string][] = [["import", "Import"], ["recipes", "Recipes"], ["history", "History"], ["settings", "Settings"]];
    tabsEl.innerHTML = tabs.map(([t, l]) => `<button class="con-tab${t === tab ? " active" : ""}" data-tab="${t}">${l}</button>`).join("");
    panelEl.innerHTML = tab === "import" ? panelImport() : tab === "recipes" ? panelRecipes() : tab === "history" ? panelHistory() : panelSettings();
  }

  tabsEl.addEventListener("click", (ev) => {
    const b = (ev.target as HTMLElement).closest<HTMLElement>("[data-tab]");
    if (!b) return;
    captureInputs();
    tab = b.dataset.tab as Tab;
    paint();
    if (tab === "recipes") void loadRecipes();
    if (tab === "history") void loadRuns();
    if (tab === "settings") void loadSettings();
  });

  panelEl.addEventListener("click", (ev) => {
    const b = (ev.target as HTMLElement).closest<HTMLElement>("[data-act]");
    if (!b) return;
    ev.preventDefault();
    captureInputs();
    void act(b.dataset.act ?? "", b.dataset);
  });

  async function act(a: string, ds: DOMStringMap): Promise<void> {
    if (a === "parse") {
      const r = await consoleImport(pasteText, pasteName.trim() || null, fold);
      if (r.error) { append("stderr", `import: ${r.error}`); return; }
      importResult = r.data;
      editorText = JSON.stringify(r.data.recipe, null, 2);
      planView = (r.data.plan as Json) ?? null;
      confirmFor = null;
      append("console", `[console] parsed the paste into recipe ${String((r.data.recipe as Json).name)} — review it in the panel`);
    } else if (a === "unfold" && importResult) {
      editorText = JSON.stringify(importResult.unfolded, null, 2);
      importResult = { ...importResult, folds: [] };
    } else if (a === "plan") {
      const doc = editorDoc();
      if (doc) {
        const r = await consolePlan(doc, parseParams(paramsText));
        planView = r.error ? null : r.data;
        if (r.error) editorErr = r.error;
      }
    } else if (a === "save") {
      const doc = editorDoc();
      if (doc) {
        const r = await consoleRecipeSave(doc, true);
        append(r.error ? "stderr" : "console", r.error ? `save: ${r.error}` : `[console] saved recipe ${String(doc.name)}`);
      }
    } else if (a === "run") {
      const doc = editorDoc();
      if (doc) await startRun(doc, []);
    } else if (a === "confirm-run" && confirmFor) {
      const doc = editorDoc();
      if (doc) await startRun(doc, confirmFor.ids);
    } else if (a === "confirm-cancel") {
      confirmFor = null;
    } else if (a === "open-recipe" && ds.name) {
      const r = await consoleRecipeGet(ds.name);
      if (!r.error) {
        const doc = { ...r.data };
        delete (doc as Json).builtin;
        editorText = JSON.stringify(doc, null, 2);
        importResult = null;
        planView = null;
        confirmFor = null;
        tab = "import";
      }
    } else if (a === "delete-recipe" && ds.name) {
      const r = await consoleRecipeDelete(ds.name);
      if (r.error) append("stderr", r.error);
      await loadRecipes();
      return;
    } else if (a === "show-run" && ds.id) {
      const r = await consoleRunResult(ds.id);
      shownRun = r.error ? null : r.data;
    } else if (a === "export-run" && ds.id) {
      const r = await consoleRunExport(ds.id);
      append(r.error ? "stderr" : "console", r.error ? `export: ${r.error}` : `[console] exported to ${String(r.data.path)}`);
    } else if (a === "save-settings" && settings) {
      const remote = panelEl.querySelector<HTMLInputElement>("#conRemote")?.checked ?? false;
      const apiShell = panelEl.querySelector<HTMLInputElement>("#conApiShell")?.checked ?? false;
      const allow = (panelEl.querySelector<HTMLTextAreaElement>("#conAllow")?.value ?? "").split("\n").map((s) => s.trim()).filter(Boolean);
      const r = await consoleSettingsSet(remote, apiShell, allow);
      settingsMsg = r.error ? `Not saved: ${r.error}` : "Saved.";
      if (!r.error) settings = { ...settings, ...r.data };
    } else if (a === "reset-allow" && settings) {
      settings = { ...settings, exec_allowlist: settings.default_exec_allowlist };
    }
    // The click handler captured the inputs before act() ran.
    paint(false);
  }

  if (!isTauri()) {
    append("console", "[console] not connected to the LLMdil service (browser preview): commands are unavailable.");
  } else {
    append("console", "[console] LLMdil debug console. Type `help` for commands, or paste a command block in Import. Nothing pasted runs without review.");
    const active = await consoleRunsList();
    if (typeof active.data.active === "string") {
      append("console", `[console] run ${active.data.active} is in progress — following it`);
      followRun(active.data.active);
    }
  }
  paint();
  lineEl.focus();

  return () => {
    disposed = true;
    if (runTimer !== null) window.clearInterval(runTimer);
    if (shellTimer !== null) window.clearInterval(shellTimer);
    if (shellOn) void consoleShellClose(sessionId);
  };
}

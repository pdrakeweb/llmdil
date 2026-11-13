/* Setup wizard view (split from static.ts). */

import { getBackends, getWizardCommands, getWizardSteps, isTauri, markSetupComplete, openUrl, runAudit, runWizardStep, wizardBusyMessage } from "../api";
import type { PlannedCommandWire } from "../api";
import { esc } from "../html";
import { icon } from "../icons";
import { META, fromWire, runLifecycle } from "../lifecycle";
import type { WizardStepState } from "../types";

/* ---------------- Setup wizard ---------------- */

/** Quote one argument for PowerShell when it contains whitespace or
 * quotes; plain tokens pass through unquoted. */
function psQuoteArg(a: string): string {
  return a === "" || /[\s"]/.test(a) ? `"${a.replace(/"/g, '\\"')}"` : a;
}

/** Render a step's planned commands as a PowerShell block the user can
 * paste into a terminal to run the step manually. Paths in the planned
 * commands are absolute, so the block works from any directory.
 * Pseudo-commands are translated to their manual equivalents
 * (download → curl.exe; http-get/http-post → Invoke-RestMethod).
 * Secret env values are never printed: the gateway master key renders
 * as a self-generated value, which makes a manual gateway run
 * self-consistent (it will not match the app's stored key — that only
 * matters when the app itself probes the manual instance). */
function formatManualCommands(cmds: PlannedCommandWire[]): string {
  const lines: string[] = [];
  for (const c of cmds) {
    if (c.argv[0] === "download" && c.argv.length === 3) {
      lines.push(`curl.exe -L -o ${psQuoteArg(c.argv[2])} ${psQuoteArg(c.argv[1])}`);
      continue;
    }
    if (c.argv[0] === "http-get" && c.argv.length >= 2) {
      lines.push(`Invoke-RestMethod ${psQuoteArg(c.argv[1])}`);
      continue;
    }
    if (c.argv[0] === "http-post" && c.argv.length >= 2) {
      const body = c.argv[2] ? ` -ContentType "application/json" -Body '${c.argv[2].replace(/'/g, "''")}'` : "";
      lines.push(`Invoke-RestMethod -Method Post -Uri ${psQuoteArg(c.argv[1])}${body}`);
      continue;
    }
    if (c.cwd) lines.push(`Set-Location ${psQuoteArg(c.cwd)}`);
    for (const k of Object.keys(c.env).sort()) {
      const v = c.env[k];
      if (k === "LITELLM_MASTER_KEY" && v.includes("credential")) {
        lines.push(`$env:LITELLM_MASTER_KEY = "sk-local-" + [guid]::NewGuid().ToString("N")  # manual-run key; the app uses its stored Credential Manager key`);
      } else {
        lines.push(`$env:${k} = "${v.replace(/"/g, '\\"')}"`);
      }
    }
    const argv = c.argv.map(psQuoteArg).join(" ");
    const needsCall = c.argv.length > 0 && /[\\/]/.test(c.argv[0]);
    lines.push(needsCall ? `& ${argv}` : argv);
    if (c.stdout_log) {
      lines.push(`# the app captures this command's output to ${c.stdout_log}${c.stderr_log ? ` and ${c.stderr_log}` : ""}`);
    }
  }
  return lines.join("\n");
}

function wizardStepRow(s: WizardStepState, live: boolean, open: boolean): string {
  // Running is a transitional state — amber, per principle 11 (the
  // segbar's running segment and .step.running match in styles.css).
  const stIcon = s.state === "done" || s.state === "manual" ? `<span style="color:var(--ok)">${icon("check")}</span>`
    : s.state === "running" ? `<span style="color:var(--warn)">${icon("clock")}</span>`
    : s.state === "failed" ? `<span style="color:var(--err)">${icon("err")}</span>`
    : `<span style="color:var(--text3)">\u25cb</span>`;
  const stateTxt = s.state === "done" ? `done \u00b7 ${esc(s.duration ?? "")}`
    : s.state === "manual" ? "manual"
    : s.state === "running" ? esc(s.note ?? "running\u2026")
    : esc(s.state);
  const helpBtn = s.state === "failed" && s.helpUrl
    ? `<button class="btn icon-only" data-help-url="${esc(s.helpUrl)}" title="Open driver download" aria-label="Open driver download">${icon("external")}</button>` : "";
  const runBtn = live && (s.state === "queued" || s.state === "failed")
    ? `<button class="btn" data-run-step="${esc(s.id)}">${s.state === "failed" ? `${icon("restart")} Retry` : `${icon("play")} Run`}</button>` : "";
  const actions = helpBtn || runBtn ? `<div style="margin-top:6px;display:flex;gap:6px">${helpBtn}${runBtn}</div>` : "";
  // The outcome's own manual block wins over the generic planned
  // commands: it is built for this install and this failure (real
  // paths, the PostgreSQL pre-step when installed, the secret env
  // named), and its first line already says what it is.
  const manualText = s.state === "failed" ? (s.manualCommand ?? s.manualCommands) : undefined;
  const hasDetails = !!(s.detail || (s.note && s.state !== "running") || (s.state === "failed" && (s.remediation || (s.logTail && s.logTail.length) || manualText)));
  /* Details expandable: ALL narrative lives here, as Label: Value
   * lines — Outcome (the step message), Note, Fix (remediation,
   * with the help URL stripped from the sentence when the Open
   * button already carries it), the log tail, and the manual
   * commands with their Copy button beside the label. */
  let details = "";
  if (open && hasDetails) {
    const fixText = s.state === "failed" && s.remediation
      ? (s.helpUrl ? s.remediation.split(s.helpUrl).join("").replace(/\s*[—–-]\s*download:\s*$/, "").replace(/download:\s*$/, "").trim() : s.remediation)
      : "";
    details = `<div class="sdetails">
      ${s.detail ? `<div><span class="k">Outcome:</span> ${esc(s.detail)}</div>` : ""}
      ${s.note && s.state !== "running" ? `<div><span class="k">Note:</span> ${esc(s.note)}</div>` : ""}
      ${fixText ? `<div><span class="k">Fix:</span> ${esc(fixText)}</div>` : ""}
      ${s.state === "failed" && s.logTail && s.logTail.length ? `<div><span class="k">Log (last 3 lines):</span> <a href="#/logs?scope=wizard" style="font-size:11px">Open in Logs view</a><div style="margin-top:2px;font-family:var(--mono);font-size:11px;color:var(--text3)">${s.logTail.slice(-3).map((l) => esc(l)).join("<br>")}</div></div>` : ""}
      ${manualText ? `<div><div style="display:flex;gap:8px;align-items:center">${s.manualCommand ? `<span></span>` : `<span class="k">Manual commands (PowerShell):</span>`}<span style="flex:1"></span><button class="btn" data-copy-step="${esc(s.id)}" title="Copy commands" aria-label="Copy commands" style="padding:2px 8px;font-size:11px">${icon("copy")}</button></div>
        <pre style="margin:4px 0;font-family:var(--mono);font-size:11px;white-space:pre-wrap;word-break:break-all;background:var(--panel);border:1px solid var(--border);border-radius:6px;padding:8px">${esc(manualText)}</pre></div>` : ""}
    </div>`;
  }
  // A done step with a note signals it with an amber info icon in
  // the row — the note text itself lives in the expandable.
  const noteIcon = s.note && s.state === "done" ? `<span class="info-tip" style="color:var(--warn)" title="${esc(s.note)}">${icon("info")}</span> ` : "";
  const chevron = hasDetails
    ? `<button class="btn" data-wiz-details="${esc(s.id)}" aria-expanded="${open}" title="Details" aria-label="Details" style="padding:2px 8px;font-size:11px">${icon(open ? "chevD" : "chevR")}</button>`
    : "<span></span>";
  return `<div class="step ${s.state}">${stIcon}
    <div><div class="sname">${esc(s.name)}</div>${actions}</div>
    <div class="sstate">${noteIcon}${stateTxt}</div>${chevron}${details}</div>`;
}

interface WizardPaintOpts {
  busy?: boolean;
  runningStepId?: string | null;
  statusLine?: string | null;
  /** A run or audit is in progress: steps without a result from
   *  this run (freshIds) render blank instead of the previous
   *  session's success/failure. */
  hideStale?: boolean;
  freshIds?: Set<string>;
  /** Steps whose Details expandable the operator opened, and steps
   *  whose auto-open (blocked step) the operator dismissed. */
  expandedSteps?: Set<string>;
  suppressAutoOpen?: Set<string>;
}

function paintWizard(root: HTMLElement, steps: WizardStepState[], live: boolean, opts: WizardPaintOpts = {}): void {
  // The backend's Running state cannot be observed mid-step (the run
  // command holds the wizard lock for the whole execution, and the
  // state query needs the same lock), so the view marks the step it
  // invoked as running itself. While a step runs, its box must not
  // keep showing the previous session's result: it renders blank,
  // labeled "running", in the amber running style — whatever its prior
  // state was. During a full run or audit (hideStale), steps that
  // have not produced a result in this run render blank as well, so
  // no stale success or failure stays on screen while work is in
  // progress.
  const blank = (s: WizardStepState, state: WizardStepState["state"]): WizardStepState => ({
    ...s, state, detail: "", note: undefined, duration: undefined,
    remediation: undefined, logTail: undefined, manualCommands: undefined, manualCommand: undefined,
  });
  const eff = steps.map((s) => {
    if (opts.runningStepId && s.id === opts.runningStepId) return blank(s, "running");
    if (opts.hideStale && !(opts.freshIds && opts.freshIds.has(s.id))) return blank(s, "queued");
    return s;
  });
  const done = eff.filter((s) => s.state === "done" || s.state === "manual").length;
  const progress = root.querySelector("#wizProgress");
  if (progress) {
    // Progress is stated once as the done count and once as the
    // segmented bar — no percentage text (numbers in one place).
    progress.innerHTML = `<b>${done}</b><span style="color:var(--text3);font-size:12px">of ${eff.length} steps</span>
      <div class="segbar">${eff.map((s) => `<i class="${s.state === "done" || s.state === "manual" ? "done" : s.state === "running" ? "run" : ""}"></i>`).join("")}</div>`
      + (opts.statusLine ? `<div style="margin-top:4px;font-size:12px;color:var(--text2)">${esc(opts.statusLine)}</div>` : "");
  }
  // The step a run is blocked on (the first failed step, when no
  // run is in flight) auto-opens its Details — unless the operator
  // dismissed it.
  const blockedId = !opts.busy ? (eff.find((s) => s.state === "failed")?.id ?? null) : null;
  const stepOpen = (s: WizardStepState): boolean =>
    (opts.expandedSteps?.has(s.id) ?? false)
    || (s.id === blockedId && !(opts.suppressAutoOpen?.has(s.id) ?? false));
  const list = root.querySelector("#wizSteps");
  if (list) {
    list.innerHTML = `<div class="sec-title">Setup checklist</div>`
      + eff.map((s) => wizardStepRow(s, live, stepOpen(s))).join("");
  }
  const banner = root.querySelector("#wizBanner") as HTMLElement | null;
  if (banner) {
    // Rule 5: the banner carries the mode as a status value only.
    // The operating instructions are not primary-UI content; in
    // sample mode the one needed fact rides the banner's tooltip.
    banner.className = live ? "banner mode-live" : "banner";
    banner.title = live ? "" : "Open the installed app to run setup against this machine.";
    banner.innerHTML = live
      ? `<span class="badge-ok">${icon("check")}</span> Live`
      : `${icon("warn")} Sample mode`;
  }
  // Busy state is painted, not remembered per element: every control
  // that can start work is disabled while a run is in flight, so the
  // state survives repaints and cannot be bypassed by clicking a
  // per-step button during a full run.
  const busy = !!opts.busy;
  for (const sel of ["#runBtn", "#auditBtn", "#finishBtn"]) {
    const b = root.querySelector(sel) as HTMLButtonElement | null;
    if (b) b.disabled = busy;
  }
  root.querySelectorAll("[data-run-step]").forEach((b) => { (b as HTMLButtonElement).disabled = busy; });
}

export function renderWizard(): string {
  const live = isTauri();
  return `<div class="topbar"><h1>Setup</h1><span class="spacer"></span><a class="btn" href="#/dashboard">Back to dashboard</a></div>
  <div class="view"><div class="banner" id="wizBanner">${icon("clock")} Loading setup state\u2026</div>
  <div class="wiz">
    <div class="wiz-head">${icon("wrench")}<h2>Set up Local LLM Service</h2><span class="spacer" style="flex:1"></span>
      <button class="btn icon-only" id="auditBtn" title="Re-run audit" aria-label="Re-run audit">${icon("search")}</button>
      <button class="btn primary" id="runBtn">${icon("play")} Run setup</button></div>
    <div class="wiz-progress" id="wizProgress"></div>
    <div class="wiz-body">
      <div class="wiz-steps" id="wizSteps"></div>
    </div>
    <div class="wiz-foot"><button class="btn icon-only" title="Back" aria-label="Back" disabled>${icon("chevL")}</button><span class="spacer"></span>
      <button class="btn primary" id="finishBtn">${icon("check")} Finish</button></div>
    <div class="logstream" id="wizLog" style="border:0;border-top:1px solid var(--border);border-radius:0"></div>
  </div></div>`;
}

export function wireWizard(root: HTMLElement): () => void {
  // The root click listener below is delegated on the persistent #view
  // element, so it must be removed on teardown — every other wire*
  // function returns its cleanup for the same reason.
  let disposed = false;
  const logEl = root.querySelector("#wizLog");
  const logLine = (text: string, cls = "lvl-info") => {
    if (!logEl) return;
    const t = new Date().toLocaleTimeString("en-GB");
    logEl.insertAdjacentHTML("beforeend", `<div class="ln"><span class="t">${t}</span> <span class="${cls}">${esc(text)}</span></div>`);
  };

  let steps: WizardStepState[] = [];
  let liveNow = false;
  let busy = false;
  let runningStepId: string | null = null;
  let statusLine: string | null = null;
  /** The most recent busy report's message (which step is running
   * and for how long), for the status line after a busy stop. */
  let busyMsg = "";
  /** While a setup run or audit is in progress, steps that have not
   *  produced a result in this run render blank (paintWizard's
   *  hideStale); freshIds collects the steps that have. */
  let hideStale = false;
  const freshIds = new Set<string>();
  /** Details-expandable state per step: opened by the operator, and
   *  auto-opens dismissed by the operator. */
  const wizExpanded = new Set<string>();
  const wizAutoSuppressed = new Set<string>();

  const paint = (): void => paintWizard(root, steps, liveNow, { busy, runningStepId, statusLine, hideStale, freshIds, expandedSteps: wizExpanded, suppressAutoOpen: wizAutoSuppressed });

  /** Manual-command text per failed step id, fetched once per step
   * from setup_get_commands and rendered by the row with a copy
   * button. An empty string marks a step whose commands could not be
   * loaded, so it is not refetched on every paint. */
  const manualCmds = new Map<string, string>();
  const enrichFailed = async (): Promise<void> => {
    if (!liveNow) return;
    let changed = false;
    for (const s of steps) {
      if (s.state !== "failed") continue;
      // The outcome's own manual block (start-gateway) is specific
      // to the failure; the planned commands would only duplicate it.
      if (s.manualCommand) continue;
      const cached = manualCmds.get(s.id);
      if (cached !== undefined) {
        if (cached && s.manualCommands !== cached) { s.manualCommands = cached; changed = true; }
        continue;
      }
      const r = await getWizardCommands(s.id);
      if (disposed) return;
      if (!r.error && r.data.length) {
        const text = formatManualCommands(r.data);
        manualCmds.set(s.id, text);
        s.manualCommands = text;
        changed = true;
      } else {
        manualCmds.set(s.id, "");
      }
    }
    if (changed) paint();
  };

  const refresh = async (): Promise<void> => {
    const r = await getWizardSteps();
    if (disposed) return;
    steps = r.data;
    liveNow = r.live;
    paint();
    if (r.error) logLine(`could not load setup state: ${r.error}`, "lvl-err");
    void enrichFailed();
  };

  /** The wizard's steps run in the wizard layer, which has no handle
   *  to the backend supervisor — only the gateway has a wizard step.
   *  Before the smoke test, start every enabled backend that is not
   *  already running: the smoke test probes through the gateway to
   *  the backends, so they must be up (the step itself then waits for
   *  their health endpoints while the models load). A backend that
   *  fails to start is logged; the smoke test reports the consequence. */
  const startEnabledBackends = async (): Promise<void> => {
    const r = await getBackends();
    if (r.error || !r.data) { logLine(`could not list backends before the smoke test: ${r.error ?? "no data"}`, "lvl-warn"); return; }
    for (const b of r.data) {
      if (!b.configured || !b.enabled) continue;
      // Live (running, starting, restarting, not responding) or mid
      // stop: nothing to start.
      const st = fromWire(b.lifecycle ?? b.state);
      if (META[st].isLive || META[st].isTransition) continue;
      logLine(`starting backend: ${b.id}\u2026`);
      const sr = await runLifecycle({ kind: "backend", id: b.id }, "start", { origin: "wizard" });
      if (!sr.ok) logLine(`backend ${b.id} did not start: ${sr.message}`, "lvl-err");
      else logLine(`backend ${b.id} starting`);
    }
  };

  const runOne = async (stepId: string): Promise<"ok" | "failed" | "busy"> => {
    if (stepId === "smoke-test") await startEnabledBackends();
    logLine(`running step: ${stepId}`);
    const r = await runWizardStep(stepId);
    if (r.error) { freshIds.add(stepId); logLine(`step ${stepId} error: ${r.error}`, "lvl-err"); await refresh(); return "failed"; }
    if (r.data.busy || !r.data.outcome) {
      busyMsg = wizardBusyMessage(r.data, "A setup step is already running — wait for it to finish.");
      logLine(busyMsg, "lvl-warn");
      await refresh();
      return "busy";
    }
    const o = r.data.outcome;
    freshIds.add(stepId);
    logLine(`step ${stepId}: ${o.state} \u2014 ${o.message}`, o.state === "done" || o.state === "manual" ? "lvl-info" : "lvl-err");
    await refresh();
    return o.state === "done" || o.state === "manual" || o.state === "skipped" ? "ok" : "failed";
  };

  const runBtn = root.querySelector("#runBtn") as HTMLButtonElement | null;
  runBtn?.addEventListener("click", async () => {
    if (!isTauri()) { alert("Sample mode: setup runs in the installed app."); return; }
    if (busy) return;
    busy = true;
    hideStale = true;
    freshIds.clear();
    paint();
    try {
      await refresh();
      // This run skips already-complete steps entirely, so their
      // state is final rather than stale: seed freshIds with them so
      // paintWizard keeps rendering them as done instead of blanking
      // them to "queued" for the whole run (which read as the run
      // resetting every completed step). Only steps that will
      // actually execute blank until they produce a result.
      for (const s of steps) {
        if (s.state === "done" || s.state === "manual" || s.state === "skipped") freshIds.add(s.id);
      }
      let paused: string | null = null;
      let busyStop = false;
      for (const s of [...steps]) {
        if (s.state === "done" || s.state === "manual" || s.state === "skipped") continue;
        runningStepId = s.id;
        statusLine = `Running step: ${s.name}\u2026`;
        paint();
        const res = await runOne(s.id);
        runningStepId = null;
        if (res === "busy") { busyStop = true; break; }
        if (res !== "ok") { paused = s.name; break; }
      }
      if (busyStop) {
        statusLine = busyMsg || "A setup step is already running \u2014 wait for it to finish.";
      } else if (paused) {
        statusLine = `Setup paused \u2014 ${paused} failed. Fix the cause and retry.`;
        logLine("setup paused \u2014 fix the failed step and retry", "lvl-warn");
      } else {
        statusLine = "Setup run finished.";
      }
    } finally {
      busy = false;
      runningStepId = null;
      hideStale = false;
      paint();
    }
  });

  const audit = root.querySelector("#auditBtn");
  audit?.addEventListener("click", async () => {
    if (!isTauri()) { alert("Sample mode: audit would re-run all checks here."); return; }
    if (busy) return;
    busy = true;
    hideStale = true;
    freshIds.clear();
    statusLine = "Running audit \u2014 re-checking all steps\u2026";
    paint();
    logLine("running audit\u2026");
    try {
      const r = await runAudit();
      if (r.error) {
        statusLine = `Audit failed: ${r.error}`;
        logLine(`audit failed: ${r.error}`, "lvl-err");
        await refresh();
        return;
      }
      if (r.data.already_running) {
        // Not a failure: an audit (often the launch-time hydration
        // audit) is already in flight. Its results land in the
        // checklist when it finishes.
        statusLine = "Audit already running \u2014 its results will appear in the checklist when it finishes.";
        logLine("audit already running");
        return;
      }
      // Paint the audit's own per-step results: refresh() would reload
      // the install wizard's state, which an audit never changes — that
      // is why the old handler appeared to do nothing.
      steps = r.data.steps;
      statusLine = `Audit complete: ${r.data.done}/${r.data.total} checks passed \u2014 the checklist shows the audit results.`;
      logLine(`audit complete: ${r.data.done}/${r.data.total} checks passed`);
      void enrichFailed();
    } finally {
      busy = false;
      hideStale = false;
      paint();
    }
  });

  root.querySelector("#finishBtn")?.addEventListener("click", async () => {
    if (busy) return;
    if (isTauri()) {
      const remaining = steps.filter((s) => !(s.state === "done" || s.state === "manual" || s.state === "skipped")).length;
      if (remaining > 0 && !confirm(`${remaining} setup step(s) are not complete. Mark setup complete anyway?`)) return;
      const r = await markSetupComplete();
      if (r.error) { alert(`Could not mark setup complete: ${r.error}`); return; }
    }
    location.hash = "#/dashboard";
  });

  const onRootClick = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;
    const detBtn = t.closest("[data-wiz-details]") as HTMLElement | null;
    if (detBtn) {
      const id = detBtn.getAttribute("data-wiz-details") ?? "";
      const isOpen = detBtn.getAttribute("aria-expanded") === "true";
      if (isOpen) {
        // Closing: if it was open only via auto-open, suppress that;
        // if the operator opened it, just close it.
        if (wizExpanded.has(id)) wizExpanded.delete(id);
        else wizAutoSuppressed.add(id);
      } else {
        wizExpanded.add(id);
        wizAutoSuppressed.delete(id);
      }
      paint();
      return;
    }
    const helpBtn = t.closest("[data-help-url]") as HTMLElement | null;
    if (helpBtn) {
      const url = helpBtn.getAttribute("data-help-url") ?? "";
      helpBtn.setAttribute("disabled", "");
      const r = await openUrl(url);
      helpBtn.removeAttribute("disabled");
      if (r.error) alert(`Could not open the download page: ${r.error}`);
      return;
    }
    const copyBtn = t.closest("[data-copy-step]") as HTMLElement | null;
    if (copyBtn) {
      const id = copyBtn.getAttribute("data-copy-step") ?? "";
      const step = steps.find((s) => s.id === id);
      const text = step?.manualCommand ?? step?.manualCommands ?? "";
      if (text) {
        try {
          await navigator.clipboard.writeText(text);
        } catch {
          // Clipboard API unavailable in this webview context: fall
          // back to a temporary textarea + execCommand.
          const ta = document.createElement("textarea");
          ta.value = text;
          document.body.appendChild(ta);
          ta.select();
          document.execCommand("copy");
          ta.remove();
        }
        statusLine = "Commands copied to the clipboard.";
        paint();
      }
      return;
    }
    const runStepBtn = t.closest("[data-run-step]") as HTMLElement | null;
    if (runStepBtn) {
      if (busy) return;
      const id = runStepBtn.getAttribute("data-run-step") ?? "";
      busy = true;
      runningStepId = id;
      statusLine = `Running step: ${steps.find((s) => s.id === id)?.name ?? id}\u2026`;
      paint();
      let res: "ok" | "failed" | "busy" = "failed";
      try {
        res = await runOne(id);
      } finally {
        busy = false;
        runningStepId = null;
        statusLine = res === "busy" ? (busyMsg || "A setup step is already running \u2014 wait for it to finish.") : null;
        paint();
      }
    }
  };
  root.addEventListener("click", onRootClick);

  void refresh();

  return () => {
    disposed = true;
    root.removeEventListener("click", onRootClick);
  };
}

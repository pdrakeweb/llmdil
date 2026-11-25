/* Settings view (split from static.ts). */

import { adminApiCreateEnrollment, adminApiCreateUser, adminApiDeleteUser, adminApiGetAudit, adminApiGetStatus, adminApiListUsers, adminApiRegenerateCert, adminApiResetPassword, adminApiSetEnabled, adminApiUpdateUser, appUpdateCheck, appUpdateDownload, appUpdateSetAuto, appUpdateSetChecking, exportDiagnostics, getAppConfig, getAppUpdateStatus, getMxcActivity, getMxcPolicy, logReceiverDeploy, logReceiverFlipEndpoint, logReceiverRemove, logReceiverStatus, mxcRunSelfTest, openFolder, sendTestNotification, servicesList, servicesRegister, servicesUnregister, setMxcMode, setNotificationConfig, setSupervisionMode, setTelemetryInterval, taskAbort, tasksList, updateAppConfig, updatesApply, updatesCheckAll, updatesPin, updatesSetAuto, winmlRecheckCapabilities } from "../api";
import type { AdminApiAuditEntry, AdminApiPairing, AdminApiStatus, AdminApiUser, AppUpdateInfo, ComponentUpdateCheck, TaskRecordWire } from "../api";
import { confirmDialog } from "../dialog";
import { portError } from "../errors";
import { esc, keepFocus } from "../html";
import { pageState } from "../viewState";
import { icon } from "../icons";
import { serviceStore, wireLifecycleActions } from "../lifecycle";
import type { LifecycleResult } from "../lifecycle";
import { bannerHtml, cadenceBucket, emptyHtml, loadingHtml, setBanner, topbar } from "./common";
import { appUpdateInner, asRecord, componentUpdatesInner, logReceiverInner, remoteAccessHtml, settingsPaint, supervisionInner, supervisionSignature } from "./settingsPanels";
import type { SettingsTab, SettingsUi } from "./settingsPanels";

export function renderSettings(): string {
  return `${topbar("Settings")}
  <div class="view"><div id="setBanner"></div>
  <div id="setMsg" class="page-msg"></div>
  <div id="setBody">${loadingHtml("Loading settings\u2026")}</div></div>`;
}

export async function wireSettings(root: HTMLElement): Promise<() => void> {
  let disposed = false;
  /** Self-test result HTML survives repaints (kept in the closure). */
  let selfTestHtml = "";
  /** Tab + MXC rules-filter/expandable state, held outside the
   *  repainted DOM and kept across visits. */
  const setUi = pageState<SettingsUi>("settings", { tab: "general", rulesFilter: "fs", rulesOpen: new Set<string>(), allOpen: false });

  let lastExportPath = "";
  /** Last loaded notifications object, so a toggle click can send
   *  the complete replacement (the command replaces the section). */
  let lastNotif: Record<string, unknown> | null = null;
  // Remote Access tab state, held outside the repainted DOM.
  let raStatus: AdminApiStatus | null = null;
  let raUsers: AdminApiUser[] = [];
  let raTemp: { user: string; password: string } | null = null;
  let raEnroll: (AdminApiPairing & { user: string }) | null = null;
  let raAudit: AdminApiAuditEntry[] = [];
  const loadRemote = async (): Promise<void> => {
    const [st, us, au] = await Promise.all([adminApiGetStatus(), adminApiListUsers(), adminApiGetAudit(20)]);
    if (disposed) return;
    raStatus = st.data;
    raUsers = us.data;
    raAudit = au.data;
  };
  const setMsg = (text: string, isErr = false): void => {
    const m = root.querySelector("#setMsg");
    if (m) m.innerHTML = text ? `<span style="color:${isErr ? "var(--err)" : "var(--text2)"}">${esc(text)}</span>` : "";
  };

  /** Last app-update status payload, for the Updates tab block and
   *  its toggle handlers (the switch sends the negation of the
   *  rendered state). */
  let lastAppUpd: AppUpdateInfo | null = null;
  let appUpdLive = false;
  /** Last component-update check and the update task this view
   *  started (polled until it settles). */
  let compCheck: ComponentUpdateCheck | null = null;
  let compTask: TaskRecordWire | null = null;
  let compPoll: number | null = null;
  const stopCompPoll = (): void => {
    if (compPoll !== null) { window.clearInterval(compPoll); compPoll = null; }
  };
  const pollCompTask = (taskId: string): void => {
    stopCompPoll();
    compPoll = window.setInterval(() => {
      void (async () => {
        const r = await tasksList();
        if (disposed) { stopCompPoll(); return; }
        const rec = r.data.find((x) => x.id === taskId) ?? null;
        if (!rec) return;
        compTask = rec;
        if (rec.status !== "running") {
          stopCompPoll();
          const chk = await updatesCheckAll();
          if (disposed) return;
          if (chk.data) compCheck = chk.data;
          setMsg(rec.status === "done" ? "Component update finished." : `Component update ${rec.status}.`, rec.status === "failed");
        }
        if (setUi.tab === "updates") await load();
      })();
    }, 3000);
  };
  /** Read token produced by the deploy that just ran in this
   *  session; shown once on the Supervision tab until the next
   *  action replaces it. */
  let lrNewToken: string | null = null;
  const load = async (): Promise<void> => {
    const [cfgR, polR, actR, svcR, updR, lrR] = await Promise.all([getAppConfig(), getMxcPolicy(), getMxcActivity(), servicesList(), getAppUpdateStatus(), logReceiverStatus()]);
    if (disposed) return;
    lastAppUpd = updR.error ? lastAppUpd : updR.data;
    appUpdLive = updR.live;
    setBanner(root, "setBanner", bannerHtml(cfgR));
    const b = root.querySelector("#setBody");
    if (!b) return;
    if (cfgR.error) { b.innerHTML = emptyHtml(`Settings unavailable: ${cfgR.error}`); return; }
    if (!cfgR.data) {
      b.innerHTML = emptyHtml(cfgR.live
        ? "The app returned no configuration."
        : "No configuration in sample mode.");
      return;
    }
    lastNotif = asRecord(cfgR.data.notifications);
    // Policy/activity CallResults go in whole: their .error renders as
    // "unavailable — <error>", distinct from genuine absence.
    if (setUi.tab === "remote") await loadRemote();
    if (disposed) return;
    const cfg = cfgR.data;
    keepFocus(b, () => { b.innerHTML = settingsPaint(cfg, polR, actR, selfTestHtml, setUi, svcR,
      remoteAccessHtml(raStatus, raUsers, raTemp, raEnroll, raAudit),
      appUpdateInner(lastAppUpd, appUpdLive),
      logReceiverInner(lrR, lrNewToken),
      componentUpdatesInner(compCheck, appUpdLive, compTask), compCheck); });
  };

  const onClick = async (ev: Event): Promise<void> => {
    const t = ev.target as HTMLElement;

    const tabBtn = t.closest("[data-set-tab]") as HTMLElement | null;
    if (tabBtn) {
      setUi.tab = (tabBtn.getAttribute("data-set-tab") ?? "general") as SettingsTab;
      await load();
      return;
    }

    const filterBtn = t.closest("[data-rules-filter]") as HTMLElement | null;
    if (filterBtn) {
      setUi.rulesFilter = (filterBtn.getAttribute("data-rules-filter") ?? "fs") as SettingsUi["rulesFilter"];
      await load();
      return;
    }

    const rulesToggle = t.closest("[data-rules-toggle]") as HTMLElement | null;
    if (rulesToggle) {
      const id = rulesToggle.getAttribute("data-rules-toggle") ?? "";
      if (setUi.rulesOpen.has(id)) setUi.rulesOpen.delete(id); else setUi.rulesOpen.add(id);
      await load();
      return;
    }

    if (t.closest("[data-all-toggle]")) {
      setUi.allOpen = !setUi.allOpen;
      await load();
      return;
    }


    const raRegen = t.closest("[data-ra-regen]") as HTMLButtonElement | null;
    if (raRegen) {
      if (!await confirmDialog({ title: "Regenerate the API certificate?", body: "The fingerprint changes and every paired device must pair again.", confirmLabel: "Regenerate", danger: true })) return;
      raRegen.disabled = true;
      const r = await adminApiRegenerateCert();
      if (disposed) return;
      if (r.error) setMsg(`Certificate regeneration failed: ${r.error}`, true);
      else setMsg("Certificate regenerated. Paired devices must pair again.");
      await load();
      return;
    }

    const raToggle = t.closest("[data-ra-toggle]") as HTMLButtonElement | null;
    if (raToggle && raStatus) {
      raToggle.disabled = true;
      const r = await adminApiSetEnabled(!raStatus.enabled);
      if (disposed) return;
      if (r.error) setMsg(`Admin API change failed: ${r.error}`, true);
      await load();
      return;
    }

    if (t.closest("[data-ra-create]")) {
      const nameEl = root.querySelector("#raUser") as HTMLInputElement | null;
      const passEl = root.querySelector("#raPass") as HTMLInputElement | null;
      const roleEl = root.querySelector("#raRole") as HTMLSelectElement | null;
      const r = await adminApiCreateUser(nameEl?.value ?? "", passEl?.value ?? "", roleEl?.value ?? "viewer");
      if (disposed) return;
      // Rule 17: a failed create keeps the operator's input —
      // no repaint, so the username/password/role fields stay
      // as typed and the error shows in #setMsg (outside the
      // repainted body). Success repaints: the form clears and
      // the new user displays in the list.
      if (r.error) { setMsg(`Create user failed: ${r.error}`, true); return; }
      setMsg(`User ${r.data?.username ?? ""} created.`);
      await load();
      return;
    }

    const raApplyRole = t.closest("[data-ra-apply-role]") as HTMLElement | null;
    if (raApplyRole) {
      const id = raApplyRole.getAttribute("data-ra-apply-role") ?? "";
      const sel = root.querySelector(`[data-ra-role-for="${id}"]`) as HTMLSelectElement | null;
      const r = await adminApiUpdateUser(id, sel?.value);
      if (disposed) return;
      // Rule 17, same as the other Settings Apply handlers: a
      // failed save does not repaint, so the chosen role stays
      // selected with the error shown; success repaints the
      // user row in its new state.
      if (r.error) { setMsg(`Role change failed: ${r.error}`, true); return; }
      await load();
      return;
    }

    const raDis = t.closest("[data-ra-toggle-disabled]") as HTMLElement | null;
    if (raDis) {
      const id = raDis.getAttribute("data-ra-toggle-disabled") ?? "";
      const u = raUsers.find((x) => x.id === id);
      const r = await adminApiUpdateUser(id, undefined, !(u?.disabled ?? false));
      if (disposed) return;
      if (r.error) setMsg(`User change failed: ${r.error}`, true);
      await load();
      return;
    }

    const raReset = t.closest("[data-ra-reset]") as HTMLElement | null;
    if (raReset) {
      const id = raReset.getAttribute("data-ra-reset") ?? "";
      const name = raReset.getAttribute("data-ra-name") ?? "";
      const r = await adminApiResetPassword(id);
      if (disposed) return;
      if (r.error) setMsg(`Password reset failed: ${r.error}`, true);
      else if (r.data) raTemp = { user: name, password: r.data };
      await load();
      return;
    }

    const raEnrollBtn = t.closest("[data-ra-enroll]") as HTMLElement | null;
    if (raEnrollBtn) {
      const id = raEnrollBtn.getAttribute("data-ra-enroll") ?? "";
      const name = raEnrollBtn.getAttribute("data-ra-name") ?? "";
      const r = await adminApiCreateEnrollment(id);
      if (disposed) return;
      if (r.error) setMsg(`Pairing code failed: ${r.error}`, true);
      else if (r.data) raEnroll = { ...r.data, user: name };
      await load();
      return;
    }

    const raDelete = t.closest("[data-ra-delete]") as HTMLElement | null;
    if (raDelete) {
      const id = raDelete.getAttribute("data-ra-delete") ?? "";
      const name = raDelete.getAttribute("data-ra-name") ?? "";
      if (!await confirmDialog({ title: `Delete user ${name}?`, body: "Their sessions and API tokens are revoked.", confirmLabel: "Delete", danger: true })) return;
      const r = await adminApiDeleteUser(id);
      if (disposed) return;
      if (r.error) setMsg(`Delete failed: ${r.error}`, true);
      await load();
      return;
    }

    const copyBtn = t.closest("[data-copy-value]") as HTMLElement | null;
    if (copyBtn) {
      const value = copyBtn.getAttribute("data-copy-value") ?? "";
      try {
        await navigator.clipboard.writeText(value);
        setMsg("Copied to the clipboard.");
      } catch {
        setMsg("Copy failed — copy the value manually.", true);
      }
      return;
    }

    const exportBtn = t.closest("#setExportBtn") as HTMLButtonElement | null;
    if (exportBtn) {
      exportBtn.disabled = true;
      const r = await exportDiagnostics();
      exportBtn.disabled = false;
      if (disposed) return;
      if (r.error) setMsg(`Export failed: ${r.error}`, true);
      else {
        setMsg(r.data.path ? `Diagnostics bundle written to ${r.data.path}.` : "Diagnostics bundle written.");
        if (r.data.path) {
          lastExportPath = r.data.path;
          const of = root.querySelector("#setOpenFolder") as HTMLButtonElement | null;
          if (of) of.style.display = "";
        }
      }
      return;
    }

    const openFolderBtn = t.closest("#setOpenFolder") as HTMLButtonElement | null;
    if (openFolderBtn) {
      if (lastExportPath) void openFolder(lastExportPath);
      return;
    }

    const stBtn = t.closest("[data-mxc-selftest]") as HTMLButtonElement | null;
    if (stBtn) {
      stBtn.disabled = true;
      const r = await mxcRunSelfTest();
      if (disposed) return;
      if (r.error) {
        selfTestHtml = `<div style="font-size:12px;color:var(--err);margin-top:8px">Self-test failed: ${esc(r.error)}</div>`;
      } else {
        const s = r.data.summary;
        const failed = r.data.cases.filter((c) => !c.passed);
        selfTestHtml = `<div style="font-size:12px;margin-top:8px">
          ${r.data.runtime_installed === false ? `<div style="color:#e3b341">MXC runtime: not installed — the probes ran against the policy only.</div>` : ""}
          <div>Self-test: ${s.passed}/${s.total} probes passed${s.failed ? ` \u2014 ${s.failed} failed` : ""}.${failed.length ? `<span class="info-tip" title="To fix a failing case, edit the policy file at the Policy file path shown above and restart the app to apply it.">${icon("info")}</span>` : ""}</div>
          ${failed.map((c) => `<div style="color:var(--err)">${esc(c.name)}: expected ${esc(c.expected)}, got ${esc(c.actual)} \u2014 ${esc(c.description)}</div>`).join("")}
        </div>`;
      }
      await load();
      return;
    }

    const telApply = t.closest("#setTelApply") as HTMLButtonElement | null;
    if (telApply) {
      const el = root.querySelector("#setTelInterval") as HTMLInputElement | null;
      const ms = Math.round(Number(el?.value));
      if (!Number.isFinite(ms) || ms < 0 || ms > 60000) {
        setMsg("Poll interval must be a number of milliseconds between 0 and 60000.", true);
        return;
      }
      telApply.disabled = true;
      const r = await setTelemetryInterval(ms);
      if (disposed) return;
      if (r.error) {
        setMsg(`Poll interval not saved: ${r.error}`, true);
        telApply.disabled = false;
        return;
      }
      const eff = cadenceBucket(ms);
      setMsg(eff.value === "0"
        ? "Telemetry polling off — saved; applies on restart."
        : `Poll interval saved — sampler cadence ${eff.label}; applies on restart.`);
      await load();
      return;
    }
    const appUpdBtn = t.closest("[data-appupd-toggle]") as HTMLButtonElement | null;
    if (appUpdBtn) {
      const key = appUpdBtn.getAttribute("data-appupd-toggle") ?? "";
      if (!lastAppUpd || !["checking", "auto"].includes(key)) return;
      appUpdBtn.disabled = true;
      const r = key === "checking"
        ? await appUpdateSetChecking(!lastAppUpd.check_enabled)
        : await appUpdateSetAuto(!lastAppUpd.auto_update);
      if (disposed) return;
      if (r.error) setMsg(`Update settings not saved: ${r.error}`, true);
      else setMsg("Update settings saved.");
      await load();
      return;
    }

    const winmlToggle = t.closest("#setWinmlToggle") as HTMLButtonElement | null;
    if (winmlToggle) {
      const on = winmlToggle.getAttribute("aria-checked") === "true";
      winmlToggle.disabled = true;
      const r = await updateAppConfig({ winml: { enabled: !on } });
      if (disposed) return;
      if (r.error) setMsg(`Windows ML change not saved: ${r.error}`, true);
      else setMsg(on ? "Windows ML disabled and saved." : "Windows ML enabled and saved.");
      await load();
      return;
    }
    if (t.closest("[data-appupd-check]")) {
      const btn = t.closest("[data-appupd-check]") as HTMLButtonElement;
      btn.disabled = true;
      setMsg("Checking GitHub Releases…");
      const r = await appUpdateCheck();
      if (disposed) return;
      if (r.error) setMsg(`Update check failed: ${r.error}`, true);
      else if (r.data.last_check_outcome === "failed") setMsg(`Update check failed: ${r.data.last_check_error ?? "unknown error"}`, true);
      else if (r.data.available) setMsg(`LLMdil ${r.data.available.version} is available.`);
      else setMsg("LLMdil is up to date.");
      await load();
      return;
    }
    if (t.closest("[data-compupd-check]")) {
      const btn = t.closest("[data-compupd-check]") as HTMLButtonElement;
      btn.disabled = true;
      setMsg("Checking component release sources…");
      const r = await updatesCheckAll();
      if (disposed) return;
      if (r.error || !r.data) {
        setMsg(`Component check failed: ${r.error ?? "no result"}`, true);
      } else {
        compCheck = r.data;
        const failed = r.data.components.filter((c) => c.check_error).map((c) => c.name);
        const avail = r.data.components.filter((c) => c.update_available).map((c) => c.name);
        setMsg(`${avail.length ? `Updates available: ${avail.join(", ")}.` : "No component updates available."}${failed.length ? ` Could not reach the release source for: ${failed.join(", ")}.` : ""}`, failed.length > 0);
      }
      await load();
      return;
    }
    const compApply = t.closest("[data-compupd-apply]") as HTMLButtonElement | null;
    const compApplyAll = t.closest("[data-compupd-apply-all]") as HTMLButtonElement | null;
    if (compApply || compApplyAll) {
      const id = compApply ? (compApply.getAttribute("data-compupd-apply") ?? "") : "all";
      const version = compApply ? (compApply.getAttribute("data-compupd-version") ?? "") : "";
      const what = id === "all" ? "every available update of an unpinned component" : `${id}${version ? ` ${version}` : ""}`;
      if (!await confirmDialog({ title: `Apply ${what}?`, body: "Dependent services (backends or the gateway) are stopped and restarted; a failed update is rolled back.", confirmLabel: "Apply" })) return;
      (compApply ?? compApplyAll)!.disabled = true;
      const r = await updatesApply(id, version || null);
      if (disposed) return;
      if (r.error || !r.data) {
        setMsg(`Update not started: ${r.error ?? "no result"}`, true);
      } else if (!r.data.task_id) {
        setMsg(r.data.message ?? "Nothing to update.");
      } else {
        compTask = { id: r.data.task_id, kind: "updates_apply", status: "running", detail: null, created_ms: Date.now() };
        setMsg(`Update started (task ${r.data.task_id}).`);
        pollCompTask(r.data.task_id);
      }
      await load();
      return;
    }
    const compAbort = t.closest("[data-compupd-abort]") as HTMLButtonElement | null;
    if (compAbort) {
      const taskId = compAbort.getAttribute("data-compupd-abort") ?? "";
      compAbort.disabled = true;
      setMsg("Cancelling the update — the component in flight is rolled back…");
      const r = await taskAbort(taskId);
      if (disposed) return;
      if (r.error) setMsg(`Cancel failed: ${r.error}`, true);
      return;
    }

    const winmlApply = t.closest("#setWinmlApply") as HTMLButtonElement | null;
    if (winmlApply) {
      const port = Number((root.querySelector("#setWinmlPort") as HTMLInputElement | null)?.value);
      const model = ((root.querySelector("#setWinmlModel") as HTMLInputElement | null)?.value ?? "").trim();
      if (portError(port)) {
        setMsg("Windows ML port must be a whole number between 1 and 65,535.", true);
        return;
      }
      winmlApply.disabled = true;
      const r = await updateAppConfig({ winml: { port, model_id: model === "" ? null : model } });
      if (disposed) return;
      if (r.error) {
        setMsg(`Windows ML settings not saved: ${r.error}`, true);
        winmlApply.disabled = false;
        return;
      }
      setMsg("Windows ML port and model saved.");
      await load();
      return;
    }

    const winmlRecheck = t.closest("#setWinmlRecheck") as HTMLButtonElement | null;
    if (winmlRecheck) {
      winmlRecheck.disabled = true;
      setMsg("Probing Windows ML…");
      const r = await winmlRecheckCapabilities();
      if (disposed) return;
      winmlRecheck.disabled = false;
      if (r.error || !r.data) {
        setMsg(`Windows ML re-check failed: ${r.error ?? "no report"}`, true);
        return;
      }
      const rep = r.data;
      const serves = rep.models.length ? rep.models.join(", ") : "no models listed";
      const split = rep.limits.supports_tensor_split === true ? "supported" : "not supported";
      setMsg(`Windows ML at ${rep.base_url}: serves ${serves} · adapter ${rep.adapter_hint ?? "not reported"} · tensor split ${split}`);
      return;
    }

    const pgApply = t.closest("#setPgApply") as HTMLButtonElement | null;
    if (pgApply) {
      const port = Number((root.querySelector("#setPgPort") as HTMLInputElement | null)?.value);
      if (portError(port)) {
        setMsg("PostgreSQL port must be a whole number between 1 and 65,535.", true);
        return;
      }
      pgApply.disabled = true;
      const r = await updateAppConfig({ postgres: { port } });
      if (disposed) return;
      if (r.error) {
        setMsg(`PostgreSQL port not saved: ${r.error}`, true);
        pgApply.disabled = false;
        return;
      }
      setMsg("PostgreSQL port saved — applies the next time PostgreSQL starts.");
      await load();
      return;
    }

    const raPortSave = t.closest("[data-ra-port-save]") as HTMLButtonElement | null;
    if (raPortSave) {
      const port = Number((root.querySelector("#raPort") as HTMLInputElement | null)?.value);
      if (portError(port)) {
        setMsg("Admin API port must be a whole number between 1 and 65,535.", true);
        return;
      }
      raPortSave.disabled = true;
      const r = await updateAppConfig({ admin_api: { port } });
      if (disposed) return;
      if (r.error) {
        setMsg(`Admin API port not saved: ${r.error}`, true);
        raPortSave.disabled = false;
        return;
      }
      setMsg("Admin API port saved — switch the API off and on, or restart the app, to apply it.");
      await load();
      return;
    }

    const raBindSave = t.closest("[data-ra-bind-save]") as HTMLButtonElement | null;
    if (raBindSave) {
      const sel = (root.querySelector("#raBind") as HTMLSelectElement | null)?.value ?? "";
      // Loopback / all keep any remembered interface address; an
      // interface choice sends the address with it.
      const patch: Record<string, unknown> = sel.startsWith("ip:")
        ? { bind_mode: "interface", bind_ip: sel.slice(3) }
        : { bind_mode: sel };
      if (!["loopback", "all", "interface"].includes(String(patch.bind_mode))) {
        setMsg("Choose where the Admin API listens.", true);
        return;
      }
      raBindSave.disabled = true;
      const r = await updateAppConfig({ admin_api: patch });
      if (disposed) return;
      if (r.error) {
        setMsg(`Admin API bind not saved: ${r.error}`, true);
        raBindSave.disabled = false;
        return;
      }
      setMsg("Admin API bind saved — switch the API off and on, or restart the app, to apply it.");
      await load();
      return;
    }

    const compAuto = t.closest("[data-comp-auto]") as HTMLButtonElement | null;
    if (compAuto) {
      const name = compAuto.getAttribute("data-comp-auto") ?? "";
      const on = compAuto.getAttribute("aria-checked") === "true";
      compAuto.disabled = true;
      const r = await updatesSetAuto(name, !on);
      if (disposed) return;
      if (r.error) setMsg(`Auto-update change for ${name} not saved: ${r.error}`, true);
      else setMsg(`Auto-update ${!on ? "on" : "off"} for ${name} — saved.`);
      await load();
      return;
    }

    const compPinSave = t.closest("[data-comp-pin-save]") as HTMLButtonElement | null;
    if (compPinSave) {
      const name = compPinSave.getAttribute("data-comp-pin-save") ?? "";
      const input = root.querySelector(`[data-comp-pin-input="${name}"]`) as HTMLInputElement | null;
      const version = (input?.value ?? "").trim();
      if (!version) {
        setMsg(`Enter a version to pin ${name} to, or clear the pin with Unpin.`, true);
        return;
      }
      compPinSave.disabled = true;
      const r = await updatesPin(name, version);
      if (disposed) return;
      if (r.error) {
        setMsg(`Pin for ${name} not saved: ${r.error}`, true);
        compPinSave.disabled = false;
        return;
      }
      setMsg(`${name} pinned to ${version} — saved.`);
      await load();
      return;
    }

    const compUnpin = t.closest("[data-comp-unpin]") as HTMLButtonElement | null;
    if (compUnpin) {
      const name = compUnpin.getAttribute("data-comp-unpin") ?? "";
      compUnpin.disabled = true;
      const r = await updatesPin(name, null);
      if (disposed) return;
      if (r.error) setMsg(`Unpin of ${name} failed: ${r.error}`, true);
      else setMsg(`${name} unpinned — saved.`);
      await load();
      return;
    }

    const mxcPathApply = t.closest("#setMxcPathApply") as HTMLButtonElement | null;
    if (mxcPathApply) {
      const path = ((root.querySelector("#setMxcPath") as HTMLInputElement | null)?.value ?? "").trim();
      if (!path) {
        setMsg("Policy file path must not be empty.", true);
        return;
      }
      mxcPathApply.disabled = true;
      const r = await updateAppConfig({ mxc: { path } });
      if (disposed) return;
      if (r.error) {
        setMsg(`Policy file path not saved: ${r.error}`, true);
        mxcPathApply.disabled = false;
        return;
      }
      setMsg("Policy file path saved — applies when the app restarts.");
      await load();
      return;
    }
    if (t.closest("[data-appupd-download]")) {
      const btn = t.closest("[data-appupd-download]") as HTMLButtonElement;
      btn.disabled = true;
      setMsg("Downloading the update…");
      const r = await appUpdateDownload();
      if (disposed) return;
      if (r.error) setMsg(`Download failed: ${r.error}`, true);
      else if (r.data.staged) setMsg(`LLMdil ${r.data.staged.version} downloaded and verified — restart from the banner to apply it.`);
      else if (r.data.last_stage_error) setMsg(`Download failed: ${r.data.last_stage_error}`, true);
      await load();
      return;
    }

    const notifBtn = t.closest("[data-notif-toggle]") as HTMLButtonElement | null;
    if (notifBtn) {
      const key = notifBtn.getAttribute("data-notif-toggle") ?? "";
      if (!lastNotif || !["enabled", "operations", "failures", "activation"].includes(key)) return;
      // Missing keys read as on (the serde defaults agree).
      const next = {
        enabled: lastNotif.enabled !== false,
        operations: lastNotif.operations !== false,
        failures: lastNotif.failures !== false,
        activation: lastNotif.activation !== false,
      };
      const k = key as keyof typeof next;
      next[k] = !next[k];
      notifBtn.disabled = true;
      const r = await setNotificationConfig(next);
      if (disposed) return;
      if (r.error) setMsg(`Notification settings not saved: ${r.error}`, true);
      else setMsg("Notification settings saved.");
      await load();
      return;
    }

    const supBtn = t.closest("[data-sup-mode]") as HTMLButtonElement | null;
    if (supBtn) {
      const m = supBtn.getAttribute("data-sup-mode") ?? "process";
      supBtn.disabled = true;
      const r = await setSupervisionMode(m);
      if (disposed) return;
      if (r.error) {
        setMsg(`Supervision mode change failed: ${r.error}`, true);
        supBtn.disabled = false;
        return;
      }
      setMsg(`Supervision mode set to ${m}.`);
      await load();
      return;
    }

    const testBtn = t.closest("[data-notif-test]") as HTMLButtonElement | null;
    if (testBtn) {
      testBtn.disabled = true;
      const r = await sendTestNotification();
      testBtn.disabled = false;
      if (disposed) return;
      if (r.error) setMsg(`Test notification failed: ${r.error}`, true);
      else setMsg("Test notification sent. If no toast appeared, check Windows notification settings for this app.");
      return;
    }

    const regBtn = t.closest("[data-svc-register]") as HTMLButtonElement | null;
    if (regBtn) {
      const target = regBtn.getAttribute("data-svc-register") ?? "";
      regBtn.disabled = true;
      setMsg(`Registering ${target} — approve the elevation prompt, then enter the account password in the Windows credential dialog…`);
      const r = await servicesRegister(target);
      if (disposed) return;
      if (r.error) {
        setMsg(`Registration of ${target} failed: ${r.error}`, true);
        regBtn.disabled = false;
        return;
      }
      setMsg(`Registered ${r.data.service_name} as ${r.data.account}.`);
      await load();
      return;
    }

    const unregBtn = t.closest("[data-svc-unregister]") as HTMLButtonElement | null;
    if (unregBtn) {
      const target = unregBtn.getAttribute("data-svc-unregister") ?? "";
      unregBtn.disabled = true;
      setMsg(`Unregistering ${target} — approve the elevation prompt…`);
      const r = await servicesUnregister(target);
      if (disposed) return;
      if (r.error) {
        setMsg(`Unregister of ${target} failed: ${r.error}`, true);
        unregBtn.disabled = false;
        return;
      }
      setMsg(`Unregistered ${r.data.service_name}; it returns to process supervision.`);
      await load();
      return;
    }

    const lrDeploy = t.closest("[data-lr-deploy]") as HTMLButtonElement | null;
    if (lrDeploy) {
      lrDeploy.disabled = true;
      lrNewToken = null;
      setMsg("Deploying the log receiver — approve the elevation prompt…");
      const r = await logReceiverDeploy();
      if (disposed) return;
      if (r.error) {
        setMsg(`Log receiver deploy failed: ${r.error}`, true);
        lrDeploy.disabled = false;
        return;
      }
      lrNewToken = r.data.read_token_new;
      setMsg(lrNewToken
        ? "Log receiver deployed and running. File the new read token shown on the Supervision tab through the Secure Vault."
        : "Log receiver deployed and running. The existing read token was kept.");
      await load();
      return;
    }

    const lrRemove = t.closest("[data-lr-remove]") as HTMLButtonElement | null;
    if (lrRemove) {
      if (!await confirmDialog({ title: "Remove the log receiver service?", body: "Its store and token files stay in the data directory.", confirmLabel: "Remove", danger: true })) return;
      lrRemove.disabled = true;
      setMsg("Removing the log receiver — approve the elevation prompt…");
      const r = await logReceiverRemove();
      if (disposed) return;
      if (r.error) {
        setMsg(`Log receiver remove failed: ${r.error}`, true);
        lrRemove.disabled = false;
        return;
      }
      lrNewToken = null;
      setMsg("Log receiver service removed. Its store and tokens remain in the data directory.");
      await load();
      return;
    }

    const lrSource = t.closest("[data-lr-source]") as HTMLButtonElement | null;
    if (lrSource) {
      const key = lrSource.getAttribute("data-lr-source") ?? "";
      const on = lrSource.getAttribute("aria-checked") === "true";
      lrSource.disabled = true;
      const r = await updateAppConfig({ log_streaming: { sources: { [key]: !on } } });
      if (disposed) return;
      if (r.error) setMsg(`Log source change not saved: ${r.error}`, true);
      else setMsg(`Log source ${key} ${on ? "off" : "on"} — saved and applied.`);
      await load();
      return;
    }

    const lrFlip = t.closest("[data-lr-flip]") as HTMLButtonElement | null;
    if (lrFlip) {
      if (!await confirmDialog({ title: "Stream logs to the local receiver?", body: "The endpoint becomes http://127.0.0.1:8473. config.json is backed up first, and the shipper restarts onto the new endpoint at once.", confirmLabel: "Set endpoint" })) return;
      lrFlip.disabled = true;
      const r = await logReceiverFlipEndpoint();
      if (disposed) return;
      if (r.error) {
        setMsg(`Endpoint flip failed: ${r.error}`, true);
        lrFlip.disabled = false;
        return;
      }
      setMsg(r.data.already
        ? "The endpoint already points at the local receiver."
        : `Endpoint set to ${r.data.endpoint} (backup: ${r.data.backup ?? "none"}). The shipper now uses it.`);
      await load();
      return;
    }

    const modeBtn = t.closest("[data-mxc-mode]") as HTMLButtonElement | null;
    if (!modeBtn) return;
    const mode = modeBtn.getAttribute("data-mxc-mode") ?? "";
    if (mode === "disabled" && !await confirmDialog({ title: "Disable the MXC sandbox?", body: "Tool execution will no longer be mediated by the policy.", confirmLabel: "Disable", danger: true })) return;
    modeBtn.disabled = true;
    const r = await setMxcMode(mode);
    if (disposed) return;
    if (r.error) {
      setMsg(`MXC mode change failed: ${r.error}`, true);
      modeBtn.disabled = false;
      return;
    }
    if (r.data.persisted === false) {
      setMsg(`MXC mode set to ${r.data.mode} for this session only \u2014 the change was not saved and will revert when the app restarts.`, true);
    } else {
      setMsg(`MXC mode set to ${r.data.mode}.`);
    }
    await load();
  };
  root.addEventListener("click", onClick);
  // Start/Stop/Restart on the Supervision rows and the Log receiver
  // card run through the shared lifecycle runner.
  const unwireLc = wireLifecycleActions(root, (r: LifecycleResult) => {
    if (disposed) return;
    setMsg(r.message, !r.ok && !r.cancelled);
  }, { origin: "ui" });

  await load();

  // While Settings is open, the shared store also polls services_list
  // and log_receiver_status (fast during transitions, immediately on
  // lifecycle events) — the Supervision tab repaints in place when a
  // service's lifecycle state changes, instead of showing the state
  // from when the view was opened (UI-7, OS-12).
  let supSig = "";
  const unsubscribeStore = serviceStore.subscribe((snap) => {
    if (disposed || setUi.tab !== "supervision" || !snap.services) return;
    const sig = supervisionSignature(snap);
    if (sig === supSig) return;
    supSig = sig;
    const svc = root.querySelector("#supSvc");
    if (svc) svc.innerHTML = supervisionInner({ data: snap.services, live: snap.live });
    const lr = root.querySelector("#supLr");
    if (lr && snap.logReceiver) lr.innerHTML = logReceiverInner({ data: snap.logReceiver, live: snap.live }, lrNewToken);
  }, { services: true });

  return () => {
    disposed = true;
    stopCompPoll();
    unwireLc();
    unsubscribeStore();
    root.removeEventListener("click", onClick);
  };
}

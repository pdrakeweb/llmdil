/* MCP Servers view (#/mcp) — MCP design §9.
   One card per persisted registry entry (AppConfig.mcp.servers, via
   mcp_list): name + status chip, Label: Value lines, and the card
   actions (Test / exposure toggle / Edit / Remove, credential
   Store). The top strip carries the three summary values once
   (principle 7): the recorded LiteLLM version, whether stdio is
   armed for the gateway, and how many servers are registered with
   it. Tests run ONLY from the explicit Test action — never on view
   load or a timer — because a passing brave_search test performs
   one real, metered tool call (design §8). */

import {
  deleteMcpServer, getBackends, getGatewayStatus, getMcpServers,
  saveMcpServer, setMcpExposure, storeMcpCredential, testMcpServer,
} from "../api";
import type {
  BackendRow, McpExposureKind, McpServerInput, McpServerRow, McpTestResult, McpTransportKind,
} from "../api";
import { esc } from "../html";
import { icon } from "../icons";
import { enableSwitch } from "../switch";

const SELECT_STYLE = "background:var(--panel);color:var(--text);border:1px solid var(--border);border-radius:8px;padding:6px 8px;font-size:12px";

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

interface CardNote { text: string; err: boolean }

type TestState =
  | { status: "testing" }
  | { status: "done"; result: McpTestResult };

/** The add/edit form's draft. Text fields hold the raw input; the
 *  list fields (args / env_keys / allowed_tools) are split on save. */
interface McpFormDraft {
  id: string;
  display_name: string;
  transport: McpTransportKind;
  exposure: McpExposureKind;
  command: string;
  args: string;
  url: string;
  env_keys: string;
  cred_ref: string;
  allowed_tools: string;
  metered: boolean;
  rpm_limit: string;
}

function emptyDraft(): McpFormDraft {
  return {
    id: "", display_name: "", transport: "stdio", exposure: "gateway",
    command: "", args: "", url: "", env_keys: "", cred_ref: "",
    allowed_tools: "", metered: false, rpm_limit: "",
  };
}

function draftFromServer(s: McpServerRow): McpFormDraft {
  return {
    id: s.id,
    display_name: s.display_name,
    transport: s.transport,
    exposure: s.exposure,
    command: s.command ?? "",
    args: s.args.join(" "),
    url: s.url ?? "",
    env_keys: s.env_keys.join(", "),
    cred_ref: s.cred_ref ?? "",
    allowed_tools: (s.allowed_tools ?? []).join(", "),
    metered: s.metered,
    rpm_limit: s.rpm_limit === null ? "" : String(s.rpm_limit),
  };
}

/** Live id validation mirroring manager_config::validate
 *  (is_valid_mcp_server_id): the pattern is ^[a-z][a-z0-9_]*$ over
 *  the value exactly as entered — the Rust validator applies no
 *  trim or lowercase normalisation, so neither does this check;
 *  an id that fails here would be rejected at save. Returns the
 *  inline error text, or "" when the id is valid. */
function mcpIdError(id: string): string {
  if (!id) return "Id is required.";
  if (!/^[a-z][a-z0-9_]*$/.test(id)) {
    return "Id must match ^[a-z][a-z0-9_]*$ — a lowercase letter, then lowercase letters, digits, or underscores.";
  }
  return "";
}

export function renderMcp(): string {
  return `${topbar("MCP Servers", `<button class="btn" id="mcpAddBtn">${icon("server")} Add server</button>`)}
  <div class="view"><div id="mcpBanner"></div>
  <div id="mcpAgg"></div>
  <div id="mcpStrip"></div>
  <div id="mcpForm"></div>
  <div id="mcpBody">${loadingHtml("Loading MCP servers\u2026")}</div></div>`;
}

export async function wireMcp(root: HTMLElement): Promise<() => void> {
  let disposed = false;
  let servers: McpServerRow[] = [];
  let backends: BackendRow[] = [];
  let litellmVersion: string | null = null;
  let meta: { live: boolean; error?: string } = { live: false };
  /** Per-server test state for THIS view session only: before any
   *  test, a non-Disabled server shows no chip (principle 11). */
  const testState = new Map<string, TestState>();
  /** Per-server notes (action outcomes) shown under the card. */
  const msgs = new Map<string, CardNote>();
  /** Server ids whose Remove button is armed for confirmation. */
  const confirmRemove = new Set<string>();
  /** Each server's last non-Disabled exposure this session: where
   *  the exposure toggle returns a Disabled server to. Seeded from
   *  the loaded registry; a server that has only ever been seen
   *  Disabled falls back to Gateway. */
  const lastActive = new Map<string, McpExposureKind>();
  let formOpen = false;
  let editingId: string | null = null;
  let draft: McpFormDraft = emptyDraft();
  let formError = "";

  const chipFor = (s: McpServerRow): string => {
    if (s.exposure === "disabled") return ` <span class="chip status-tag status-off">disabled</span>`;
    const t = testState.get(s.id);
    if (!t) return "";
    if (t.status === "testing") return ` <span class="chip status-tag status-transition">testing</span>`;
    if (!t.result.ok) return ` <span class="chip status-tag status-bad">failed</span>`;
    // A passing Gateway test means the server is registered with
    // the gateway; a passing Local test is a liveness pass only.
    return s.exposure === "gateway"
      ? ` <span class="chip status-tag status-on">registered</span>`
      : ` <span class="chip status-tag status-on">passed</span>`;
  };

  const line = (label: string, valueHtml: string): string =>
    `<div style="font-size:12px;color:var(--text2);margin-top:4px">${label}: <span style="color:var(--text)">${valueHtml}</span></div>`;

  const cardHtml = (s: McpServerRow): string => {
    const t = testState.get(s.id);
    const testing = t?.status === "testing";
    const result = t?.status === "done" ? t.result : null;
    const lines: string[] = [];
    lines.push(line("Transport", esc(s.transport)));
    if (s.transport === "stdio") {
      const cmd = [s.command ?? "", ...s.args].join(" ").trim();
      if (cmd) lines.push(line("Command", `<span style="font-family:var(--mono)">${esc(cmd)}</span>`));
    } else if (s.url) {
      // URLs carry a copy button (principle 10), following the
      // settings rows' data-copy-value pattern.
      lines.push(line("URL", `<span style="font-family:var(--mono)">${esc(s.url)}</span> <button class="btn" data-mcp-copy="${esc(s.url)}" style="padding:1px 7px;font-size:11px">Copy</button>`));
    }
    if (s.cred_ref) {
      lines.push(line("Credential", s.credential_present ? "present" : "missing"));
    }
    // Tools are known only after a Test this session returned a
    // list (principle 12: unmeasured renders as absent, not 0).
    if (result && result.tools) lines.push(line("Tools", String(result.tools.length)));
    if (s.id === "brave_search") lines.push(line("Provider", "Brave (paid API)"));
    if (s.id === "brave_search" && s.metered) lines.push(line("Cost", "$5 per 1,000 requests"));
    if (s.rpm_limit !== null) lines.push(line("Rate cap", `${s.rpm_limit}/min`));
    if (s.allowed_tools && s.allowed_tools.length) {
      lines.push(line("Allowed tools", esc(s.allowed_tools.join(", "))));
    }
    const detailHtml = result
      ? `<div style="font-size:12px;margin-top:8px;color:${result.ok ? "var(--text2)" : "var(--err)"}">${esc(result.detail)}</div>
        ${result.real_call_ok !== undefined ? `<div style="font-size:12px;margin-top:4px;color:${result.real_call_ok ? "var(--text2)" : "var(--err)"}">Real call: ${result.real_call_ok ? "ok" : "failed"}${result.real_call_detail ? ` \u2014 ${esc(result.real_call_detail)}` : ""}</div>` : ""}`
      : "";
    // Enable/disable is the shared state switch (rule 16): ON is
    // the server's active exposure, OFF is Disabled; the click
    // keeps cycling between the two through the same handler and
    // command. The full three-state choice lives in the edit form.
    const restore = lastActive.get(s.id) ?? "gateway";
    const toggle = enableSwitch({
      on: s.exposure !== "disabled",
      name: `MCP server ${s.id}`,
      attrs: `data-mcp-act="exposure" data-mcp-id="${esc(s.id)}"`,
      title: s.exposure === "disabled"
        ? `${s.id}: disabled — turn on to ${restore === "gateway" ? "register with the gateway" : "run locally"}`
        : `${s.id}: enabled — ${s.exposure === "gateway" ? "registered with the gateway" : "running locally"}; turn off to disable`,
    });
    const credRow = s.cred_ref
      ? `<div style="display:flex;gap:8px;align-items:center;margin-top:10px;flex-wrap:wrap">
          <input type="password" data-mcp-cred-input placeholder="${esc(s.cred_ref)}" autocomplete="off" style="${SELECT_STYLE};flex:1;min-width:180px" aria-label="Credential value for ${esc(s.id)}">
          <button class="btn" data-mcp-act="store-cred" data-mcp-id="${esc(s.id)}">${icon("shield")} Store</button>
        </div>`
      : "";
    const note = msgs.get(s.id);
    return `<div class="card" data-mcp-card="${esc(s.id)}">
      <h3>${icon("server")} ${esc(s.display_name)}${chipFor(s)}</h3>
      ${lines.join("")}
      ${detailHtml}
      ${credRow}
      <div class="card-actions">
        <button class="btn" data-mcp-act="test" data-mcp-id="${esc(s.id)}"${testing ? " disabled" : ""}>${icon("bolt")} Test</button>
        ${toggle}
        <button class="btn icon-only" data-mcp-act="edit" data-mcp-id="${esc(s.id)}" title="Edit server" aria-label="Edit ${esc(s.id)}">${icon("gear")}</button>
        <button class="btn danger${confirmRemove.has(s.id) ? " armed" : ""}" data-mcp-act="remove" data-mcp-id="${esc(s.id)}">${icon("trash")} ${confirmRemove.has(s.id) ? "Confirm remove" : "Remove"}</button>
      </div>
      ${note ? `<div style="margin-top:8px;font-size:12px;color:${note.err ? "var(--err)" : "var(--text2)"}">${esc(note.text)}</div>` : ""}
    </div>`;
  };

  const paintForm = (): void => {
    const host = root.querySelector("#mcpForm");
    if (!host) return;
    if (!formOpen) { host.innerHTML = ""; return; }
    const idErr = editingId ? "" : mcpIdError(draft.id);
    const stdioGroup = draft.transport === "stdio" ? "" : "display:none";
    const httpGroup = draft.transport === "http" ? "" : "display:none";
    host.innerHTML = `<div class="card" style="margin-bottom:12px"><h3>${icon("server")} ${editingId ? `Edit server \u2014 ${esc(editingId)}` : "Add server"}</h3>
    <div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:10px">
      <label style="font-size:12px;color:var(--text2)">Id<br>
        <input data-mcp-field="id" type="text" value="${esc(draft.id)}" style="${SELECT_STYLE};width:100%"${editingId ? " disabled" : ""}>
        <span data-mcp-id-error style="color:var(--err);font-size:11px">${esc(idErr)}</span></label>
      <label style="font-size:12px;color:var(--text2)">Display name<br>
        <input data-mcp-field="display_name" type="text" value="${esc(draft.display_name)}" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Transport<br>
        <select data-mcp-field="transport" style="${SELECT_STYLE};width:100%">
          <option value="stdio"${draft.transport === "stdio" ? " selected" : ""}>stdio</option>
          <option value="http"${draft.transport === "http" ? " selected" : ""}>http</option>
        </select></label>
      <label style="font-size:12px;color:var(--text2)">Exposure<br>
        <select data-mcp-field="exposure" style="${SELECT_STYLE};width:100%">
          <option value="gateway"${draft.exposure === "gateway" ? " selected" : ""}>Gateway</option>
          <option value="local"${draft.exposure === "local" ? " selected" : ""}>Local</option>
          <option value="disabled"${draft.exposure === "disabled" ? " selected" : ""}>Disabled</option>
        </select></label>
    </div>
    <div data-mcp-stdio-group style="${stdioGroup}">
      <div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:10px">
        <label style="font-size:12px;color:var(--text2)">Command<span class="info-tip" title="Allowed launchers (design \u00a73.2): npx, uvx, python, python3, node, docker, deno">${icon("info")}</span><br>
          <input data-mcp-field="command" type="text" value="${esc(draft.command)}" placeholder="npx" style="${SELECT_STYLE};width:100%"></label>
        <label style="font-size:12px;color:var(--text2)">Args<br>
          <input data-mcp-field="args" type="text" value="${esc(draft.args)}" placeholder="-y @example/server@1.0.0" style="${SELECT_STYLE};width:100%"></label>
      </div>
    </div>
    <div data-mcp-http-group style="${httpGroup}">
      <label style="font-size:12px;color:var(--text2);display:block;margin-top:10px">URL<br>
        <input data-mcp-field="url" type="text" value="${esc(draft.url)}" placeholder="MCP server URL (https, or http on loopback)" style="${SELECT_STYLE};width:100%"></label>
    </div>
    <div style="display:grid;grid-template-columns:1fr 1fr;gap:10px;margin-top:10px">
      <label style="font-size:12px;color:var(--text2)">Env key names (comma-separated)<br>
        <input data-mcp-field="env_keys" type="text" value="${esc(draft.env_keys)}" placeholder="BRAVE_API_KEY" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Credential target<br>
        <input data-mcp-field="cred_ref" type="text" value="${esc(draft.cred_ref)}" placeholder="local-llm-service-manager/brave-search-mcp" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Allowed tools (comma-separated, optional)<br>
        <input data-mcp-field="allowed_tools" type="text" value="${esc(draft.allowed_tools)}" style="${SELECT_STYLE};width:100%"></label>
      <label style="font-size:12px;color:var(--text2)">Rate limit, rpm (optional)<br>
        <input data-mcp-field="rpm_limit" type="number" min="1" value="${esc(draft.rpm_limit)}" style="${SELECT_STYLE};width:100%"></label>
    </div>
    <label style="font-size:12px;color:var(--text2);display:block;margin-top:10px" title="The upstream API is paid; the card shows the cost line and the rate cap applies.">
      <input data-mcp-field="metered" type="checkbox"${draft.metered ? " checked" : ""}> Metered (paid upstream)</label>
    ${formError ? `<div style="font-size:12px;color:var(--err);margin-top:8px">${esc(formError)}</div>` : ""}
    <div class="card-actions"><button class="btn" data-mcp-act="form-save"${idErr ? " disabled" : ""}>${icon("check")} Save server</button>
      <button class="btn" data-mcp-act="form-cancel">Cancel</button></div>
  </div>`;
  };

  const paint = (): void => {
    const banner = root.querySelector("#mcpBanner");
    if (banner) {
      banner.innerHTML = meta.error
        ? `<div class="banner" style="margin-bottom:12px">${icon("err")} Backend error \u2014 ${esc(meta.error)}</div>`
        : !meta.live
          ? `<div class="banner" style="margin-bottom:12px">${icon("warn")} Sample mode \u2014 open the installed app for live data from this machine.</div>`
          : "";
    }
    // Aggregate banner (§9): shown only in the one bad state —
    // at least one backend is enabled and none of the enabled
    // backends has a verified tool-call probe. No banner when a
    // verified backend exists, and none when nothing is enabled.
    const agg = root.querySelector("#mcpAgg");
    if (agg) {
      const enabled = backends.filter((b) => b.enabled);
      const anyVerified = enabled.some((b) => b.tool_call_probe?.outcome === "verified");
      agg.innerHTML = enabled.length && !anyVerified
        ? `<div class="banner" style="margin-bottom:12px">${icon("warn")} Tool calls: no enabled backend has verified tool calls<span class="info-tip" title="MCP tools reach a model only if its backend emits structured tool calls. Probe each backend from its card in the Backends view (Test tool calls) \u2014 a backend whose probe is unparsed or failed will never fire a tool.">${icon("info")}</span> <a href="#/backends" style="color:var(--accent)">Backends</a></div>`
        : "";
    }
    // Strip derivation (one line, Label: Value):
    // - LiteLLM: the §4.3 probe version recorded at the last
    //   gateway spawn; "not measured" when none was recorded.
    // - Stdio: enabled iff at least one server is armed for the
    //   gateway — exposure Gateway AND stdio transport AND (no
    //   env_keys OR credential_present) — the manager-wizard
    //   arming rule, computed from the mcp_list data.
    // - Registered: the count of Gateway-exposed servers (the
    //   configured gateway set; arming is resolved at spawn).
    const strip = root.querySelector("#mcpStrip");
    if (strip) {
      const stdioArmed = servers.some((s) =>
        s.exposure === "gateway" && s.transport === "stdio"
        && (s.env_keys.length === 0 || s.credential_present));
      const registered = servers.filter((s) => s.exposure === "gateway").length;
      strip.innerHTML = `<div style="display:flex;gap:18px;flex-wrap:wrap;font-size:12px;color:var(--text2);margin-bottom:12px">
        <span>LiteLLM: <b style="color:var(--text)">${esc(litellmVersion ?? "not measured")}</b></span>
        <span>Stdio: <b style="color:var(--text)">${stdioArmed ? "enabled" : "disabled"}</b></span>
        <span>Registered: <b style="color:var(--text)">${registered}</b></span>
      </div>`;
    }
    paintForm();
    const body = root.querySelector("#mcpBody");
    if (body) {
      body.innerHTML = servers.length
        ? `<div class="grid2" style="margin-top:0">${servers.map(cardHtml).join("")}</div>`
        : emptyHtml(meta.live
          ? "No MCP servers in the registry. Add one with the Add server button."
          : "No MCP server data in sample mode.");
    }
  };

  const load = async (): Promise<void> => {
    const [sr, br, gr] = await Promise.all([getMcpServers(), getBackends(), getGatewayStatus()]);
    if (disposed) return;
    servers = sr.data;
    backends = br.data;
    litellmVersion = gr.data.litellm_version;
    meta = { live: sr.live, error: sr.error };
    for (const s of servers) {
      if (s.exposure !== "disabled") lastActive.set(s.id, s.exposure);
    }
    paint();
  };

  const closeForm = (): void => {
    formOpen = false;
    editingId = null;
    formError = "";
    paintForm();
  };

  const saveForm = async (): Promise<void> => {
    const splitList = (v: string): string[] =>
      v.split(",").map((x) => x.trim()).filter((x) => x.length > 0);
    // Args are whitespace-tokenized on save — the same convention
    // as a backend's raw_flags (values containing spaces are not
    // supported).
    const args = draft.args.split(/\s+/).filter((x) => x.length > 0);
    const allowed = splitList(draft.allowed_tools);
    const rpm = draft.rpm_limit.trim() === "" ? null : Number(draft.rpm_limit);
    const server: McpServerInput = {
      id: draft.id.trim(),
      display_name: draft.display_name.trim() || draft.id.trim(),
      transport: draft.transport,
      exposure: draft.exposure,
      command: draft.command.trim() === "" ? null : draft.command.trim(),
      args,
      url: draft.url.trim() === "" ? null : draft.url.trim(),
      env_keys: splitList(draft.env_keys),
      cred_ref: draft.cred_ref.trim() === "" ? null : draft.cred_ref.trim(),
      allowed_tools: allowed.length ? allowed : null,
      metered: draft.metered,
      rpm_limit: rpm !== null && Number.isFinite(rpm) && rpm > 0 ? rpm : null,
    };
    const r = await saveMcpServer(server);
    if (disposed) return;
    if (r.error) {
      // The command's validation message, inline; the form stays
      // open with the operator's input intact.
      formError = r.error;
      paintForm();
      return;
    }
    closeForm();
    await load();
  };

  const onClick = async (ev: Event): Promise<void> => {
    const target = ev.target as HTMLElement;
    const copyBtn = target.closest("[data-mcp-copy]") as HTMLElement | null;
    if (copyBtn) {
      const value = copyBtn.getAttribute("data-mcp-copy") ?? "";
      try { await navigator.clipboard.writeText(value); } catch { /* clipboard unavailable */ }
      return;
    }
    if (target.closest("#mcpAddBtn")) {
      if (formOpen && editingId === null) { closeForm(); return; }
      draft = emptyDraft();
      editingId = null;
      formError = "";
      formOpen = true;
      paintForm();
      return;
    }
    const t = target.closest("[data-mcp-act]") as HTMLElement | null;
    if (!t || (t as HTMLButtonElement).disabled) return;
    const act = t.getAttribute("data-mcp-act") ?? "";
    const id = t.getAttribute("data-mcp-id") ?? "";

    if (act === "form-cancel") { closeForm(); return; }
    if (act === "form-save") { await saveForm(); return; }

    const server = servers.find((s) => s.id === id);
    if (!server) return;

    if (act === "edit") {
      draft = draftFromServer(server);
      editingId = server.id;
      formError = "";
      formOpen = true;
      paintForm();
      return;
    }
    if (act === "test") {
      // The explicit Test action — the only caller of
      // testMcpServer in this view (§8).
      testState.set(id, { status: "testing" });
      paint();
      const r = await testMcpServer(id);
      if (disposed) return;
      testState.set(id, {
        status: "done",
        result: r.error ? { id, ok: false, detail: r.error } : r.data,
      });
      paint();
      return;
    }
    if (act === "exposure") {
      const next: McpExposureKind = server.exposure === "disabled"
        ? (lastActive.get(id) ?? "gateway")
        : "disabled";
      const r = await setMcpExposure(id, next);
      if (disposed) return;
      if (r.error) msgs.set(id, { text: `Exposure not changed: ${r.error}`, err: true });
      else msgs.delete(id);
      await load();
      return;
    }
    if (act === "remove") {
      if (!confirmRemove.has(id)) {
        confirmRemove.add(id);
        msgs.set(id, { text: "Click Confirm remove to delete this server from the registry.", err: false });
        paint();
        return;
      }
      confirmRemove.delete(id);
      const r = await deleteMcpServer(id);
      if (disposed) return;
      if (r.error) msgs.set(id, { text: `Remove failed: ${r.error}`, err: true });
      testState.delete(id);
      await load();
      return;
    }
    if (act === "store-cred") {
      const card = t.closest("[data-mcp-card]");
      const input = card?.querySelector("[data-mcp-cred-input]") as HTMLInputElement | null;
      const value = input?.value ?? "";
      if (!value.trim()) {
        msgs.set(id, { text: "Enter the credential value first.", err: true });
        paint();
        return;
      }
      const r = await storeMcpCredential(id, value);
      // The value is cleared and never rendered back, whatever the
      // outcome.
      if (input) input.value = "";
      if (disposed) return;
      if (r.error) {
        msgs.set(id, { text: `Credential not stored: ${r.error}`, err: true });
        paint();
        return;
      }
      msgs.set(id, { text: "Credential stored.", err: false });
      // Reload so the card's Credential line reflects the store.
      await load();
    }
  };

  /** Form field sync: draft state lives outside the repainted DOM
   *  (the static views' flags-draft pattern), so repaints keep
   *  typed input. The id field validates in place — error text and
   *  the Save button's disabled state update without a repaint. */
  const syncDraft = (ev: Event): void => {
    const t = ev.target as HTMLInputElement;
    const field = t.getAttribute("data-mcp-field");
    if (!field || !formOpen) return;
    if (field === "metered") { draft.metered = t.checked; return; }
    if (field === "id") draft.id = t.value;
    else if (field === "display_name") draft.display_name = t.value;
    else if (field === "command") draft.command = t.value;
    else if (field === "args") draft.args = t.value;
    else if (field === "url") draft.url = t.value;
    else if (field === "env_keys") draft.env_keys = t.value;
    else if (field === "cred_ref") draft.cred_ref = t.value;
    else if (field === "allowed_tools") draft.allowed_tools = t.value;
    else if (field === "rpm_limit") draft.rpm_limit = t.value;
    else if (field === "transport" && ev.type === "change") {
      draft.transport = t.value === "http" ? "http" : "stdio";
      paintForm();
      return;
    } else if (field === "exposure" && ev.type === "change") {
      draft.exposure = (["gateway", "local", "disabled"] as const).includes(t.value as McpExposureKind)
        ? (t.value as McpExposureKind) : draft.exposure;
      return;
    } else return;
    if (field === "id" && !editingId) {
      const err = mcpIdError(draft.id);
      const errEl = root.querySelector("[data-mcp-id-error]");
      if (errEl) errEl.textContent = err;
      const saveBtn = root.querySelector("[data-mcp-act=\"form-save\"]") as HTMLButtonElement | null;
      if (saveBtn) saveBtn.disabled = err !== "";
    }
  };

  root.addEventListener("click", onClick);
  root.addEventListener("input", syncDraft);
  root.addEventListener("change", syncDraft);

  await load();

  return () => {
    disposed = true;
    root.removeEventListener("click", onClick);
    root.removeEventListener("input", syncDraft);
    root.removeEventListener("change", syncDraft);
  };
}

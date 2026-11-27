import type { BackendToolCallTestResult } from "./backends";
import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";

/* ---------------- MCP servers (persisted registry) ---------------- */

export type McpTransportKind = "stdio" | "http";
export type McpExposureKind = "gateway" | "local" | "disabled";

/** One mcp_list entry: a persisted registry server (the
 *  McpServerConfig fields) plus live status — supervised-process
 *  `running` (Local servers only) and credential presence over the
 *  accepted Credential Manager targets (presence only; values are
 *  never read). `credential_targets` from the Phase 4 brief does not
 *  exist in the payload: the found target rides in
 *  `credential_target_found`, and per-env-key presence in
 *  `credential_presence`. */
export interface McpServerRow {
  id: string;
  display_name: string;
  /** Same value as display_name (back-compat name field). */
  name: string;
  transport: McpTransportKind;
  exposure: McpExposureKind;
  /** Derived runtime mode: remote | local | disabled. */
  mode: string;
  command: string | null;
  args: string[];
  url: string | null;
  env_keys: string[];
  cred_ref: string | null;
  /** Registration / API-key page for the credential, when known. */
  signup_url: string | null;
  /** What credential to store (kind + where it comes from),
   *  resolved by the backend (stored value, else the seed's for
   *  the same id). Labels the card's credential field. */
  credential_hint: string | null;
  allowed_tools: string[] | null;
  metered: boolean;
  rpm_limit: number | null;
  running: boolean;
  /** Supervised-process state ("stopped" | "running" | "failed" ...);
   *  "failed" once a Local server exited on its own. Optional: older
   *  backends do not send it. */
  process_state?: string;
  /** Why the server cannot run (a policy or a missing prerequisite), or
   *  null. Absent on older payloads. */
  blocked_reason?: string | null;
  credential_present: boolean;
  /** "present" | "missing" — the §9 card wording. */
  credential_status: string;
  /** The accepted Credential Manager target that was found, when
   *  one was (legacy servers accept either the unified wizard
   *  target or the legacy llm-manager/mcp/<id> target). */
  credential_target_found: string | null;
  /** Per-env-key presence as [env key, present] pairs. */
  credential_presence: [string, boolean][];
}

/** An MCP server definition as mcp_save_server accepts it: exactly
 *  the persisted McpServerConfig serde shape (transport and
 *  exposure are the lowercase tags; command/args serve stdio, url
 *  serves http). */
export interface McpServerInput {
  id: string;
  display_name: string;
  transport: McpTransportKind;
  exposure: McpExposureKind;
  command: string | null;
  args: string[];
  url: string | null;
  env_keys: string[];
  cred_ref: string | null;
  signup_url: string | null;
  credential_hint: string | null;
  allowed_tools: string[] | null;
  metered: boolean;
  rpm_limit: number | null;
}

/** Result of mcp_test_connection. Two payload shapes exist, keyed
 *  on the server's exposure:
 *  - Gateway: { id, ok, registered, tools, detail } — plus, for the
 *    curated servers after a passing test, real_call_ok /
 *    real_call_detail (brave_search's one real, metered call) or
 *    real_call_skipped (web_fetch never makes a real call).
 *  - Local / Disabled: { id, ok, detail } — a supervised-process
 *    liveness check (Local) or an unconditional not-ok (Disabled);
 *    the Gateway-only fields stay absent. */
export interface McpTestResult {
  id: string;
  ok: boolean;
  detail: string;
  registered?: boolean;
  tools?: string[];
  real_call_ok?: boolean;
  real_call_detail?: string;
  real_call_skipped?: boolean;
}

/** Sample-mode registry mirroring the four seeded servers, so the
 *  MCP view renders under the browser smoke test. */
function sampleMcpServers(): McpServerRow[] {
  const base = {
    name: "",
    mode: "disabled",
    command: null as string | null,
    args: [] as string[],
    url: null as string | null,
    env_keys: [] as string[],
    cred_ref: null as string | null,
    signup_url: null as string | null,
    credential_hint: null as string | null,
    allowed_tools: null as string[] | null,
    metered: false,
    rpm_limit: null as number | null,
    running: false,
    credential_present: false,
    credential_status: "missing",
    credential_target_found: null as string | null,
    credential_presence: [] as [string, boolean][],
  };
  return [
    {
      ...base, id: "github", display_name: "GitHub", name: "GitHub",
      transport: "stdio", exposure: "local", mode: "local",
      command: "npx", args: ["-y", "@modelcontextprotocol/server-github"],
      env_keys: ["GITHUB_PERSONAL_ACCESS_TOKEN"],
      cred_ref: "local-llm-service-manager/github-mcp",
      credential_hint: "GitHub personal access token — created under GitHub Settings, Developer settings, Personal access tokens",
      running: true, credential_present: true, credential_status: "present",
      credential_target_found: "local-llm-service-manager/github-mcp",
      credential_presence: [["GITHUB_PERSONAL_ACCESS_TOKEN", true]],
    },
    {
      ...base, id: "gdrive", display_name: "Google Drive", name: "Google Drive",
      transport: "stdio", exposure: "local", mode: "local",
      command: "npx", args: ["-y", "@modelcontextprotocol/server-gdrive"],
      env_keys: ["GOOGLE_APPLICATION_CREDENTIALS"],
      cred_ref: "local-llm-service-manager/gdrive-mcp",
      credential_hint: "Path to the Google OAuth credentials JSON file — downloaded from Google Cloud Console, APIs & Services, Credentials; store the file path, not the file contents",
      credential_present: true, credential_status: "present",
      credential_target_found: "local-llm-service-manager/gdrive-mcp",
      credential_presence: [["GOOGLE_APPLICATION_CREDENTIALS", true]],
    },
    {
      ...base, id: "brave_search", display_name: "Brave Search", name: "Brave Search",
      transport: "stdio", exposure: "gateway", mode: "remote",
      command: "npx",
      args: ["-y", "@brave/brave-search-mcp-server@2.1.4", "--transport", "stdio"],
      env_keys: ["BRAVE_API_KEY"],
      cred_ref: "local-llm-service-manager/brave-search-mcp",
      credential_hint: "Brave Search API key — the subscription key from the Brave Search API page",
      // The live entry resolves the Brave Search API page here;
      // the literal URL stays out of the sample fixture because
      // the smoke test bans external URLs in the bundle.
      signup_url: null,
      allowed_tools: ["brave_web_search", "brave_llm_context"],
      metered: true, rpm_limit: 20,
      credential_presence: [["BRAVE_API_KEY", false]],
    },
    {
      ...base, id: "web_fetch", display_name: "Web Fetch", name: "Web Fetch",
      transport: "stdio", exposure: "disabled", mode: "disabled",
      command: "uvx", args: ["mcp-server-fetch@2026.10.10"],
      allowed_tools: ["fetch"],
    },
  ];
}

/** mcp_list — the persisted MCP registry with live status. */
export async function getMcpServers(): Promise<CallResult<McpServerRow[]>> {
  if (!isTauri()) return { data: sampleMcpServers(), live: false };
  try {
    const data = await invoke<McpServerRow[]>("mcp_list");
    return { data: Array.isArray(data) ? data : [], live: true };
  } catch (e) {
    return { data: [], live: true, error: errOf(e) };
  }
}

/** mcp_test_connection — test one server (see McpTestResult for the
 *  per-exposure payload shapes). For brave_search a passing test
 *  performs one real, metered tool call — this wrapper is only ever
 *  invoked from the view's explicit Test action, never on load or a
 *  timer (design §8). */
export async function testMcpServer(id: string): Promise<CallResult<McpTestResult>> {
  if (!isTauri()) return { data: { id, ok: false, detail: "sample mode — tests run in the installed app" }, live: false };
  try {
    const data = await invoke<McpTestResult>("mcp_test_connection", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { id, ok: false, detail: "" }, live: true, error: errOf(e) };
  }
}

/** mcp_save_server — create or update (upsert by id) a registry
 *  server. The command validates the whole config on save (§3.2);
 *  a violation comes back as `error` and nothing is stored. */
export async function saveMcpServer(server: McpServerInput): Promise<CallResult<{ id: string; persisted: boolean }>> {
  if (!isTauri()) return { data: { id: server.id, persisted: false }, live: false };
  try {
    const data = await invoke<{ id: string; persisted: boolean }>("mcp_save_server", { server });
    return { data, live: true };
  } catch (e) {
    return { data: { id: server.id, persisted: false }, live: true, error: errOf(e) };
  }
}

/** mcp_delete_server — remove a registry server (a running Local
 *  server is stopped first). */
export async function deleteMcpServer(id: string): Promise<CallResult<{ id: string; deleted: boolean; persisted: boolean }>> {
  if (!isTauri()) return { data: { id, deleted: true, persisted: false }, live: false };
  try {
    const data = await invoke<{ id: string; deleted: boolean; persisted: boolean }>("mcp_delete_server", { id });
    return { data, live: true };
  } catch (e) {
    return { data: { id, deleted: false, persisted: false }, live: true, error: errOf(e) };
  }
}

/** mcp_set_exposure — set a server's exposure (gateway | local |
 *  disabled), persisted through the validated save path. */
export async function setMcpExposure(id: string, exposure: McpExposureKind): Promise<CallResult<{ id: string; exposure: string; persisted: boolean }>> {
  if (!isTauri()) return { data: { id, exposure, persisted: false }, live: false };
  try {
    const data = await invoke<{ id: string; exposure: string; persisted: boolean }>("mcp_set_exposure", { id, exposure });
    return { data, live: true };
  } catch (e) {
    return { data: { id, exposure, persisted: false }, live: true, error: errOf(e) };
  }
}

/** mcp_store_credential — store a server's credential in Windows
 *  Credential Manager under its declared cred_ref target. The value
 *  is passed to the command only; it is never returned or rendered. */
export async function storeMcpCredential(id: string, value: string): Promise<CallResult<{ id: string; stored: boolean }>> {
  if (!isTauri()) return { data: { id, stored: false }, live: false };
  try {
    const data = await invoke<{ id: string; stored: boolean }>("mcp_store_credential", { id, value });
    return { data, live: true };
  } catch (e) {
    return { data: { id, stored: false }, live: true, error: errOf(e) };
  }
}

/** One mcp_start_preflight entry: a Gateway-exposed MCP server a
 *  gateway start would omit for want of a credential — the store
 *  it via mcp_store_credential before the start and the start's
 *  arming pass includes it in that same start. */
export interface McpStartPreflightServer {
  id: string;
  display_name: string;
  cred_ref: string;
  env_keys: string[];
  signup_url: string | null;
  credential_hint: string | null;
  metered: boolean;
}

/** mcp_start_preflight — the enabled-but-unarmed Gateway MCP
 *  servers, for the start-gateway credential prompt
 *  (ui/src/mcp-prompt.ts). Empty when every Gateway server's
 *  credential is present. */
export async function mcpStartPreflight(): Promise<CallResult<{ servers: McpStartPreflightServer[] }>> {
  if (!isTauri()) return { data: { servers: [] }, live: false };
  try {
    const data = await invoke<{ servers: McpStartPreflightServer[] }>("mcp_start_preflight");
    return { data: { servers: Array.isArray(data?.servers) ? data.servers : [] }, live: true };
  } catch (e) {
    return { data: { servers: [] }, live: true, error: errOf(e) };
  }
}

/** backend_test_tool_call — probe a backend for structured
 *  tool_calls (MCP design §7.2); the outcome persists on the
 *  backend and renders on its card as the Tool calls line. */
export async function testBackendToolCall(id: string): Promise<CallResult<BackendToolCallTestResult>> {
  const fallback: BackendToolCallTestResult = { outcome: "failed", finish_reason: null, chat_template_tool_use: false, chat_template_caps: null, detail: "", latency_ms: 0 };
  if (!isTauri()) return { data: fallback, live: false, error: "backend_test_tool_call is not available in sample mode" };
  try {
    const data = await invoke<BackendToolCallTestResult>("backend_test_tool_call", { id });
    return { data, live: true };
  } catch (e) {
    return { data: fallback, live: true, error: errOf(e) };
  }
}

# MCP Capabilities — Design

Status: shipped. Phases 0–4 (§12) were implemented and merged on
2026-10-17 and are in releases from v0.1.36, present in v0.1.40;
Phase 5 (on-machine validation) remains Pete's. v0.1.39 (tracker
#44) added `McpServerConfig.signup_url`, the `mcp_start_preflight`
op, and a pre-start credential prompt in the UI. Two design points
did **not** ship as written, and the sections below are annotated
where they occur: per-key MCP grants (§5.3 — the generator emits
`require_key_mcp_access_defined: true`, but the app has no
virtual-key issuance surface, so it issues no `object_permission`
grants), and the §3.1 schema's exact shape (§3.1 note). This design supersedes the
Google-supplied blueprint "Integrating Web Search MCP Servers with
LightLLM", which a council review rejected on 2026-10-17 (verdict:
DO NOT SHIP; log `~/workspace/council_log_2026-10-17.md`). The
blueprint targeted the wrong product (LightLLM, an unrelated
ModelTC/Triton project — Prometheus runs LiteLLM), named an npm
package that does not exist, pinned an archived Brave server, and
proposed a custom Python MCP host duplicating the MCP gateway
LiteLLM already ships. This design is the re-scope the council's
Judge directed: gateway-native, built on the manager's existing MCP
surface, with the security controls specified up front.

## Scope

In scope:

- Registering MCP servers with the LiteLLM gateway the app already
  runs, so the tools are callable by gateway clients.
- A persisted MCP server registry in the app config, replacing the
  current hardcoded definitions and session-only mode overlay.
- Manager UI for server management, credential presence, testing,
  and status. The manager has no chat UI; no chat experience is
  designed here.
- Two curated servers at launch: Brave web search (official server)
  and URL fetch. SearXNG via LiteLLM's built-in search provider as
  an optional item (§6.3).
- A per-backend tool-call validation probe, so a backend that
  cannot emit structured tool calls is visibly marked instead of
  silently never firing (§7).

Out of scope:

- A custom MCP host process, in any language. LiteLLM is the host.
- Registering MCP servers in LiteLLM's database via its admin API.
  The generated `config.yaml` is the only registration channel
  (§1).
- Writing MCP blocks into editor configuration files. Editors
  consume the gateway's `/mcp` endpoint directly (§1.3).
- MCP endpoints in the admin REST API (the admin API plan already
  excludes MCP from its v1).
- Sandboxing MCP server processes with MXC. The wizard records
  that as configuration only today; it stays unchanged.

## Key decisions

| Decision | Choice |
|---|---|
| MCP host | LiteLLM's native MCP Gateway. No custom host (§1). |
| Registration channel | `mcp_servers:` block in the generated `config.yaml`. Database registration is not used (§1). |
| Source of truth | `AppConfig.mcp.servers`. The gateway YAML, the spawn environment, and the UI all derive from it (§3, §10). |
| Server names | Underscore-only identifiers, validated at save. LiteLLM rejects hyphenated `mcp_servers` keys at startup (§3). |
| Stdio enablement | The manager sets `LITELLM_ENABLE_MCP_STDIO=true` in the gateway spawn environment whenever an enabled stdio server is gateway-registered (§4). |
| Public exposure | The generator writes `available_on_public_internet: false` for every server. It is not a user setting (§4, §5). |
| Search server | Brave's official `@brave/brave-search-mcp-server`, version-pinned, stdio transport (§6.1). |
| Fetch server | Python `mcp-server-fetch` via `uvx`, version-pinned, described as URL fetching. Disabled by default (§6.2). |
| Secrets | Windows Credential Manager only. The YAML carries `os.environ/` references; values are resolved into the spawn environment at gateway start (§4, §5). |
| Backend precondition | `backend_spec` adds `--jinja` explicitly and filters `--no-jinja` from raw flags; a probe verifies each backend actually emits `tool_calls` (§7). |
| Bad-config safety | Three gates before a restart can take the gateway down: save-time validation, generation-time failure, and a LiteLLM version floor check; cancel regenerates the YAML from the restored config (§10). |

## 1. Architecture

### 1.1 Components

```
AppConfig.mcp.servers            (source of truth, persisted)
        |
        +--> manager-gateway     generates the mcp_servers: block
        |    in config.yaml      (enabled, gateway-exposed servers only)
        |
        +--> manager-wizard      start-gateway step: resolves secrets
        |    into the spawn env, sets LITELLM_ENABLE_MCP_STDIO,
        |    verifies the LiteLLM version floor
        |
        +--> src-tauri + ui      MCP view: manage, test, status

LiteLLM gateway (existing process, port 4000)
        +-- /mcp, /{server}/mcp        external MCP clients
        +-- /mcp-rest/tools/list|call REST tool access + interop check
        +-- /v1/chat/completions,     LLM clients; tool loop executed
            /v1/responses               by LiteLLM or by the client (§5.3)
        +-- stdio children: brave_search (npx), web_fetch (uvx)
```

Nothing else changes shape. Backends remain llama-server processes
behind the gateway; MCP servers registered for the gateway are
children of the LiteLLM process, not of the app supervisor. The two
legacy servers (GitHub, Google Drive) stay app-supervised local
servers for editor use, as today (§10.1).

### 1.2 Why config-file registration, not the database

LiteLLM accepts MCP servers from `config.yaml` and from its
database (admin UI/API). The app already treats the generated YAML
as a pure function of `AppConfig`: it is regenerated on every
gateway start, compared for drift, and snapshotted in tests
(`crates/manager-gateway/src/lib.rs:618`). Config-file registration
inherits all of that — drift detection, the activation journal,
and rollback — for free. Database registration would create a
second store the app does not generate, cannot diff, and whose
precedence against the config file for MCP servers specifically
is not documented (Investigation D, gap 1). One store, generated,
wins.

### 1.3 Consumer surfaces in v1

Named concretely, because the rejected blueprint designed a chat
UX for a product with no chat UI:

1. **External MCP clients** (Claude Desktop, Cursor, VS Code,
   Continue, Cline) connect to the gateway's `/mcp` endpoint (or
   `/{server}/mcp` for one server) with a LiteLLM virtual key
   (`x-litellm-api-key`). Tool names are namespaced by LiteLLM as
   `<server>-<tool>`. This is also how editors get MCP tools —
   they point at the gateway instead of spawning their own server
   copies.
2. **LLM clients of the gateway** (chat completions and Responses
   API) can pass `tools: [{type: "mcp", server_url:
   "litellm_proxy", …}]`; LiteLLM fetches the tools, offers them
   to the model on a llama.cpp backend, and either executes the
   calls itself or returns them to the client, per
   `require_approval` (§5.3).
3. **The manager UI** is the management surface only: server
   list, enable/disable, credentials, test, status, and the
   per-backend tool-call verification state (§7, §9).

### 1.4 What exists today (audit summary)

> Historical: this audit describes the tree **before**
> implementation (2026-10-17). The gaps below were the design's
> starting point; they are closed by the shipped implementation
> (see the status note at the top of this document) and the
> file:line citations no longer describe the current tree.

The design starts from less than the spec promised; the gaps are
load-bearing and are closed in §3, §8, and §10:

- Definitions are hardcoded: `manager_mcp::default_servers()`
  returns exactly two servers (`github`, `gdrive`), both stdio via
  `npx -y` on archived-era `@modelcontextprotocol/server-*`
  packages (`crates/manager-mcp/src/lib.rs:57`).
- There is no MCP section in `AppConfig`
  (`crates/manager-config/src/lib.rs:38`). Modes are a session-only
  in-memory overlay (`src-tauri/src/main.rs:349`); a restart
  resets them.
- Three commands exist — `mcp_list`, `mcp_test_connection`,
  `mcp_set_mode` (`src-tauri/src/main.rs:5677`, `:5726`, `:5773`).
  No UI calls them. The "test" does not speak MCP: Local checks
  process liveness, Remote is a 5 s HTTP GET.
- The wizard `mcp-servers` step writes
  `<data_dir>/mcp/mcp-servers.json` with credential references
  (`crates/manager-wizard/src/steps.rs:4932`). Nothing reads that
  file back.
- `editor_mcp_config()` generates the Continue/Cline `mcpServers`
  JSON but is dead code — called only by its own crate tests
  (`crates/manager-mcp/src/lib.rs:227`).
- Documented credential injection at spawn is unimplemented:
  `process_spec_for` builds `env: Vec::new()`
  (`crates/manager-mcp/src/lib.rs:149`), so locally spawned
  servers start without their credentials.
- The wizard and the registry use different credential targets
  (`local-llm-service-manager/github-mcp` vs
  `llm-manager/mcp/github`) and, for Drive, different env keys
  (`GOOGLE_APPLICATION_CREDENTIALS` vs `GDRIVE_CREDENTIALS_PATH`).
- The gateway generator emits exactly four sections and no
  `mcp_servers` (`crates/manager-gateway/src/lib.rs:618`); no
  `LITELLM_ENABLE_MCP_STDIO` exists anywhere in the repo.
- LiteLLM is **not pinned in code**: `litellm_spec()` installs
  unpinned `litellm[proxy]` unless the operator set
  `updates.components["litellm"].pinned_version`
  (`crates/manager-wizard/src/steps.rs:2848`). The repo's
  field-verified version is 1.105.1 (comments at
  `crates/manager-wizard/src/steps.rs:2990` and
  `crates/manager-gateway/src/lib.rs:718`).

## 2. Council compliance

Each blocking finding of the council verdict, and the provision
that answers it. Sections referenced are this document's.

| Council finding | Design provision |
|---|---|
| Blueprint targets LightLLM (wrong product, wrong port, no auth) | Design targets the deployed LiteLLM gateway on port 4000 with master-key/virtual-key auth throughout (§1). |
| Custom Python host duplicates LiteLLM's MCP Gateway | No custom host. LiteLLM is the host; the manager only generates config and spawn environment (§1). |
| `@modelcontextprotocol/server-fetch` does not exist on npm; fetch does not search | Fetch is the Python `mcp-server-fetch` via `uvx`, specified and labelled as URL fetching (§6.2). |
| Brave package archived, unpinned, `npx -y` auto-executes latest | Brave's official `@brave/brave-search-mcp-server` at a pinned version in `args` (§6.1). Same pinning rule for fetch (§6.2). |
| Hyphenated server keys rejected by LiteLLM at startup | Server ids are validated `^[a-z][a-z0-9_]*$` at save; curated ids are `brave_search`, `web_fetch` (§3). |
| Stdio servers silently disabled without the env flag | Manager sets `LITELLM_ENABLE_MCP_STDIO=true` in the spawn env when an enabled stdio server is registered; the flag is fingerprint-visible so drift detection sees it (§4). |
| `available_on_public_internet` defaults to true | Generator writes `false` for every server; not a user setting (§4). |
| Stdio config = arbitrary command execution (CVE-2026-30623 surface) | Save-time validation mirrors LiteLLM's command allowlist; a start-time version floor (≥ v1.83.7-stable) guarantees LiteLLM's own allowlist and runtime re-validation are present (§4, §5.1). |
| SSRF: model-chosen fetch URLs reach LAN services | Fetch's own default refusal of non-routable addresses is preserved — `--allow-private-ips` is a banned token in validation — and fetch ships disabled by default (§5.2, §6.2). |
| Prompt injection: fetched content fed back with no gate | Tool execution is gated by key grants and per-server tool trimming; the approval posture is an explicit decision with a recommended default (§5.3, §13b). |
| API key plaintext in a shareable JSON config | Secrets live only in Credential Manager; the YAML holds `os.environ/` references; values exist only in the gateway process environment (§4, §5.4). |
| Unbounded metered API cost | Per-server `mcp_rpm_limit` on manager-issued keys, per-key budgets, tool trimming; the absence of any LiteLLM loop-iteration cap is stated, not papered over (§5.5). |
| "Free tier" premise false | Cost model stated in §6.1: $5 per 1,000 requests, $5 monthly credit, credit card required. |
| SearXNG named but never specified | SearXNG is specified completely (container, `settings.yml`, JSON format) and routed through LiteLLM's built-in search provider, because no maintained SearXNG MCP server exists (§6.3). |
| Chat UX designed for a product with no chat UI | Consumer surfaces named in §1.3; the manager surface is a management view (§9). |
| `--jinja`/template precondition never set or verified | `--jinja` is set explicitly for every backend, `--no-jinja` is filtered from raw flags, and a probe verifies structured `tool_calls` per backend (§7). |
| No migration path; a bad MCP config can take the gateway down at startup | Unified registry with a one-time migration (§10.1); three validation gates and a cancel path that regenerates the YAML (§10.2). |
| Protocol-version interop assumed, never checked | Compatibility is gated empirically: the test flow lists tools through the gateway's own REST endpoints at every start and on demand (§8). |
| Reference implementation defects (single round, no routing, crashes) | There is no reference implementation in this design; the tool loop is executed by LiteLLM or by the calling client, both maintained code paths (§1.3, §5.3). |
| No operations story (health, logs, failure visibility) | §11: per-server load status from the gateway, existing gateway logs, explicit failure-mode table. |

## 3. Manager config schema

### 3.1 New section

`AppConfig` gains one field, serde-defaulted so existing config
files load unchanged:

```rust
// crates/manager-config
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
}

pub struct McpServerConfig {
    pub id: String,               // ^[a-z][a-z0-9_]*$ — underscore only
    pub display_name: String,
    pub transport: McpTransport,  // Stdio | Http (existing enum, manager-mcp)
    pub exposure: McpExposure,    // Gateway | Local | Disabled
    pub command: Option<String>,  // stdio; allowlist-validated
    pub args: Vec<String>,        // stdio; pinned package versions for curated servers
    pub url: Option<String>,      // http
    pub env_keys: Vec<String>,    // env var NAMES the server needs; never values
    pub cred_ref: Option<String>, // Credential Manager target for env_keys' values
    pub allowed_tools: Option<Vec<String>>, // emitted as LiteLLM allowed_tools
    pub metered: bool,            // paid upstream; drives rpm cap + UI cost labelling
    pub rpm_limit: Option<u32>,   // emitted into key generation defaults (§5.5)
}
```

`McpExposure` replaces the session-only `McpMode` overlay for
persistence: `Gateway` (registered in the LiteLLM YAML), `Local`
(app-supervised, the legacy behaviour for `github`/`gdrive`), and
`Disabled`. `manager_config::validate` covers the new section on
every save, like every other section.

**As-shipped deltas from the schema above** (in
`crates/manager-config/src/lib.rs`): `McpTransport` is defined in
`manager-config` itself, not reused from `manager-mcp` — the
crate layering forbids the dependency direction the sketch
assumed, and the serde shape is identical. `McpServerConfig`
carries one additional field, `signup_url: Option<String>`
(v0.1.39, tracker #44): where the user obtains the server's
credential, surfaced by `mcp_list` and by the `mcp_start_preflight`
op's pre-start credential prompt, and never emitted into the
generated YAML. `McpConfig` also carries the §6.3 SearXNG settings
(`searxng: SearxngSettings`), which did ship: generation emits the
top-level `search_tools:` block and the spawn environment gains
`SEARXNG_API_BASE` when it is enabled.

### 3.2 Validation rules (save-time, first gate)

Enforced in `manager_config::validate` and in the save command:

- `id` matches `^[a-z][a-z0-9_]*$` and is unique. This makes the
  LiteLLM startup rejection for hyphenated keys unreachable.
- For stdio servers, `command`'s basename is in the manager's
  mirror of LiteLLM's allowlist: `npx, uvx, python, python3,
  node, docker, deno`.
- `args` must not contain `--allow-private-ips` (any server) or
  `--ignore-robots-txt` (fetch). Banned tokens are rejected, not
  stripped, so the operator sees the refusal.
- For http servers, `url` must parse and use `https` off-loopback;
  loopback `http` is allowed (the Brave HTTP transport case).
- A `Gateway`-exposed server with `env_keys` must name a
  `cred_ref`; absence of the credential itself is a runtime state
  (card shows "Credential: missing"), not a save error.

### 3.3 Curated defaults

Seeded by the migration (§10.1) and by catalog-style defaults for
new installs:

| id | transport | command / args | env | default exposure |
|---|---|---|---|---|
| `brave_search` | stdio | `npx -y @brave/brave-search-mcp-server@<pinned> --transport stdio` | `BRAVE_API_KEY` via `local-llm-service-manager/brave-search-mcp` | Gateway, enabled when the credential is present |
| `web_fetch` | stdio | `uvx mcp-server-fetch@<pinned>` | none | Gateway, **Disabled** until explicitly enabled (§13d) |
| `github` | stdio | existing definition | existing | Local (unchanged) |
| `gdrive` | stdio | existing definition | existing | Local (unchanged) |

`<pinned>` is an exact package version chosen at build time and
recorded in the design's implementation phase (§12); bumping it
is a code change, never a runtime resolution. `npx -y` is kept
(non-interactive install) but resolves only the pinned version.

For `brave_search`, `allowed_tools` defaults to
`[brave_web_search, brave_llm_context]`, emitted both as the
server's `BRAVE_MCP_ENABLED_TOOLS` environment (the server's own
trimming mechanism) and as LiteLLM `allowed_tools`. The other six
Brave tools (local, video, image, news, summarizer, place) stay
off until the operator edits the server.

**Implementation record (Phase 3, 2026-10-17).** The `<pinned>`
versions above were resolved from the primary registries on
2026-10-17 and are recorded here per this section's rule:
`@brave/brave-search-mcp-server@2.1.4` (npm, `dist-tags.latest`
for `@brave/brave-search-mcp-server`) and
`mcp-server-fetch@2026.10.10` (PyPI, latest release of the
date-versioned `mcp-server-fetch` project). The pins live in code
as `BRAVE_SEARCH_PACKAGE_PIN` / `WEB_FETCH_PACKAGE_PIN` in
`crates/manager-config`. Phase 3 implementation notes:
`brave_search`'s `rpm_limit` (20) is stored on the entry and
surfaced via `mcp_list` but is NOT emitted into the generated
YAML — LiteLLM's `mcp_rpm_limit` is a key/team property, not a
server-config property (§5.5's enforcement attaches at key
issuance, which this app does not perform; see the Phase 2
record). SearXNG's `search_tools:` shape was verified against
LiteLLM's SearXNG search docs (`docs.litellm.ai/docs/search/searxng`):
a list entry with `search_tool_name: searxng-search` and a
`litellm_params` block carrying `search_provider: searxng` and
`api_base`. The §8 real tool call uses the documented
`POST /mcp-rest/tools/call` body (`{server_id, name, arguments}`
with the unprefixed tool name — `docs.litellm.ai/docs/mcp_rest_api`).

## 4. Gateway config generation and spawn environment

### 4.1 The `mcp_servers:` block

`generate_config_yaml` (`crates/manager-gateway/src/lib.rs:618`)
appends one block after `general_settings`, and only when at
least one server is `Gateway`-exposed — with none, the output is
byte-identical to today's, so the existing snapshot fixture and
every non-MCP install are untouched:

```yaml
mcp_servers:
  brave_search:
    transport: "stdio"
    command: "npx"
    args: ["-y", "@brave/brave-search-mcp-server@<pinned>", "--transport", "stdio"]
    env:
      BRAVE_API_KEY: "os.environ/BRAVE_API_KEY"
    available_on_public_internet: false
    allowed_tools: ["brave_web_search", "brave_llm_context"]
  web_fetch:
    transport: "stdio"
    command: "uvx"
    args: ["mcp-server-fetch@<pinned>"]
    available_on_public_internet: false
```

Rules the generator enforces regardless of stored config:
`available_on_public_internet: false` on every entry (not read
from config at all); `transport` always explicit (LiteLLM's YAML
default is `http`, its API default is `sse` — omitting it is a
latent bug); secret-bearing env values are always emitted as
`os.environ/<NAME>` references, never literals.

### 4.2 Spawn environment

In `spawn_command_with_env` (`crates/manager-wizard/src/steps.rs:3218`),
next to the unconditional `LITELLM_MASTER_KEY` insert (:3228):

- `LITELLM_ENABLE_MCP_STDIO=true` — inserted when at least one
  enabled stdio server is gateway-registered. LiteLLM reads this
  flag from the process environment only; a config or database
  setting has no effect (LiteLLM MCP docs, 2026-10-17).
- One env entry per secret of each gateway-registered server:
  the value is read from the server's `cred_ref` via the existing
  `credential_read` → `CredReadW` path and inserted under its
  env-key name — the same pattern as `OPENROUTER_API_KEY`
  (:3262). If a credential cannot be read, that server is
  omitted from the generated YAML for this start and the step
  logs a warning naming the server ("not armed"), mirroring the
  OpenRouter behaviour; the gateway itself is never failed by a
  missing MCP credential.
- The manual-start block (`crates/manager-wizard/src/steps.rs:2672`)
  gains the matching `$env:LITELLM_ENABLE_MCP_STDIO` line and a
  per-server credential note, so a manual start behaves like an
  app start.
- `planned_commands` lists the flag with a placeholder value,
  per the repo rule that planned commands equal spawned ones.
- `GatewaySpawnFingerprint` (`crates/manager-wizard/src/lib.rs:660`)
  gains an `mcp_stdio_enabled: bool` field and a hash of the
  registered server set, so toggling a server registers as drift
  and a gateway started without the flag is not adopted as
  current.

### 4.3 LiteLLM version floor

The CVE-2026-30623 allowlist and runtime re-validation are
present from LiteLLM v1.83.7-stable (BerriAI security post,
2026-04-21); the MCP gateway itself predates v1.74.0. The repo
pins neither fact today (§1.4), so the design adds a probe
instead of assuming:

- A `litellm_version_command` in the start-gateway step mirrors
  the existing prisma probe (`importlib.metadata.version`,
  `crates/manager-wizard/src/steps.rs:3622`) and records the
  installed version into the spawn-state record.
- If any stdio server is gateway-registered and the installed
  version is below **1.83.7**, the step fails closed before
  spawning, with the remediation in the step log: set
  `updates.components["litellm"].pinned_version` to a version at
  or above the floor (or clear a stale pin) and re-run. Below
  the floor the gateway is not started with MCP stdio at all —
  the failure mode the CVE describes is an allowlist-less spawn,
  and no configuration of this feature is allowed to reach it.
- The detected version is shown in the MCP view (§9) as
  `LiteLLM: <version>` and in the Gateway view's spawn state.
  Unknown version renders as "not measured", never as a guess
  (design principle 12).

## 5. Security controls

### 5.1 Command execution

Two layers, both required. The manager validates `command`
against the allowlist at save (§3.2), so a bad definition cannot
be stored. LiteLLM re-validates at spawn for config-rehydrated
servers (part of the CVE-2026-30623 fix), guaranteed present by
the §4.3 floor. The manager never sets
`LITELLM_MCP_STDIO_EXTRA_COMMANDS`; the allowlist stays at
LiteLLM's default seven commands.

### 5.2 Fetch and SSRF

The named threat is a model-chosen URL reaching a LAN service
(the gateway's own port 4000, router admin pages, Home
Assistant) with the result returned into the conversation. The
controls, in order of strength:

1. `mcp-server-fetch`'s own default: it refuses any host —
   including redirect hops and the robots.txt fetch — that
   resolves to a non-globally-routable address (loopback,
   RFC1918, link-local including 169.254.169.254, shared space,
   multicast). That guard is the primary control, and the design
   preserves it absolutely: `--allow-private-ips` is a banned
   token (§3.2). (Server README, accessed 2026-10-17.)
2. Fetch ships **Disabled**; enabling it is a separate explicit
   act (§13d).
3. Fetch is trimmed at the gateway: it has exactly one tool, and
   it gets its own `mcp_rpm_limit` when enabled.

Stated plainly, the residual: fetch cannot restrict *which
public hosts* it fetches — no allowlist mechanism exists in the
server, and LiteLLM's `allowed_params` restricts parameter
names, not values. Any public URL the model names will be
fetched, and page content is prompt-injection surface (§5.3).
The server's own README also names a DNS-rebinding
check-then-connect gap. These residuals are why fetch is opt-in
rather than default.

### 5.3 Prompt injection and the approval decision

LiteLLM's `require_approval` is a client-supplied field with
exactly two behaviours: `"never"` means LiteLLM executes the
tool loop server-side and returns the finished answer; any
other value (or omission) means tool calls are returned to the
client to review and execute. There is no human prompt inside
the proxy and no per-tool approval setting (LiteLLM MCP usage
docs, 2026-10-17). So "approval gating" in this design is not a
proxy switch — it is which keys are granted MCP access at all,
and which tools each server exposes:

- The generator adds `require_key_mcp_access_defined: true` to
  `general_settings` when any server is gateway-registered:
  a virtual key with no explicit MCP grant then gets **no** MCP
  servers, instead of LiteLLM's open-by-default every-server.
  (Shipped: `crates/manager-gateway` emits the key exactly so.)
- Manager-issued keys carry an explicit `object_permission`
  grant per server the operator enables for that key.
  (**Not shipped:** per-key MCP grants are not implemented. The
  app has no virtual-key issuance surface, so no code issues
  `object_permission` grants; the `require_key_mcp_access_defined`
  gate above is the part that shipped. The same gap is why
  `brave_search`'s `rpm_limit` is stored and displayed but not
  gateway-enforced — §5.5's enforcement attaches at key issuance.)
- Server-side trimming (§3.3) limits what any granted caller
  can reach: Brave exposes two tools, not eight.
- Direct `/mcp` clients approve calls in their own UI; that is
  the client's surface and LiteLLM documents it as such.

Decision §13b asks Pete to confirm this default (explicit
grants) against the alternative (open to all key holders, the
LiteLLM default).

### 5.4 Secrets

One rule, already the app's pattern for the master key and
OpenRouter: secret values exist only in Credential Manager and
in the gateway process environment. Config stores `env_keys`
(names) plus a `cred_ref` (a target name). YAML stores
`os.environ/` references. Logs and step plans show
`<set from credential manager>` placeholders. Spawn-state stores
SHA-256 hashes, never values (`crates/manager-wizard/src/lib.rs:682`).
The diagnostics bundle therefore contains no MCP secret, and a
config file the user shares exposes target names only — which
is why the retired wizard file's `<credential-manager:…>`
placeholder format (§10.1) was already safe and is not a leak
vector.

### 5.5 Cost and rate bounds

What LiteLLM documents (2026-10-17): per-server
`mcp_rpm_limit` per key/team, and key/team budgets that cover
MCP calls. What it does **not** document: any cap on tool-call
rounds in the server-side loop, any parallel-call cap, any MCP
call timeout. The design therefore bounds cost at the layers
that exist and does not claim a loop bound it cannot enforce:

- `brave_search` is `metered: true` with a default
  `rpm_limit` of 20 requests/minute per key — comfortably under
  the Answers plan's documented 2 requests/second, and a hard
  ceiling under any plan. Manager-issued keys inherit it.
- `brave_llm_context` calls are additionally bounded by the
  tool's own token caps (the operator can lower
  `maximum_number_of_tokens` via `allowed_params`-adjacent
  server args in a later iteration; v1 relies on rpm + budget).
- A client that loops (its own agent loop against `/mcp`, or
  repeated `require_approval: "never"` calls) is bounded by the
  same rpm limit and the key's budget, not by a round counter.
  If Brave spend ever surprises, the kill switch is one toggle:
  set `brave_search` exposure to Disabled and apply.

## 6. The servers

### 6.1 `brave_search` — Brave official MCP server

- Package: `@brave/brave-search-mcp-server` (Brave's own repo;
  the blueprint's `@modelcontextprotocol/server-brave-search`
  is archived and unmaintained). Version-pinned in `args` (§3.3).
- Transport: stdio. The server also offers Streamable HTTP, but
  its HTTP endpoint is unauthenticated and binds 127.0.0.1:8080
  by default; stdio keeps the process inside the gateway's own
  lifecycle with no extra listening port. HTTP mode is a
  documented alternative if stdio interop ever fails (§8).
- Tools: 8 exist; v1 exposes `brave_web_search` and
  `brave_llm_context` (pre-extracted, token-capped web content
  for grounding) via `BRAVE_MCP_ENABLED_TOOLS` plus LiteLLM
  `allowed_tools` (§3.3).
- Cost model, stated for the operator in the UI (§9): Brave has
  no free query tier. Current offer is $5 per 1,000 requests
  with a $5 monthly credit; a credit card is required even for
  the credit. Search-plan rate limits (50 QPS in secondary
  roundups) were not verifiable from a primary page in this
  pass; the Answers plan's 2 requests/second is documented.
  The key's actual plan is confirmed in the Brave dashboard at
  setup (§14).
- Quota visibility: LiteLLM tracks spend per key; the Brave
  dashboard is authoritative for credit balance. The app has no
  documented way to read the remaining credit, so the UI shows
  the rpm cap and spend where LiteLLM reports it, and shows the
  credit balance as not measured — never as a number it invented
  (principle 12).
- Exhaustion behaviour: Brave API errors return through the
  gateway as tool errors to the calling client. The gateway,
  other servers, and all model routing are unaffected. The
  server card's next test (§8) shows the failure with the API's
  own message.

### 6.2 `web_fetch` — URL fetching

- Package: `mcp-server-fetch` (modelcontextprotocol/servers,
  Python), run as `uvx mcp-server-fetch@<pinned>`. One tool,
  `fetch`: takes a URL, returns extracted markdown
  (`max_length` default 5000, pageable via `start_index`).
- It is specified, named, and labelled in the UI as **URL
  fetching**. It is not a search tool and is never described as
  one.
- Default state Disabled (§5.2, §13d). Its SSRF controls and
  residuals are §5.2.
- Windows note from the server's docs: stdio env carries
  `PYTHONIOENCODING: utf-8` for the fetch server entry.

### 6.3 SearXNG — via LiteLLM's built-in search provider

Decision: SearXNG is **not** an MCP server in this design. No
official or maintained SearXNG MCP server exists (all wrappers
found are unaffiliated single-maintainer projects), and running
unvetted third-party code as a gateway child is exactly the
supply-chain posture the council rejected. LiteLLM separately
ships SearXNG as a built-in **search provider** — a supported
BerriAI path that needs no MCP wrapper:

- Gateway config gains a `search_tools:` entry
  (`search_provider: searxng`, `api_base: <instance URL>`)
  alongside, not inside, the `mcp_servers:` block; the spawn env
  gains `SEARXNG_API_BASE`. The provider answers
  `POST /v1/search/searxng_search` for clients using LiteLLM's
  Search API. It is **not** exposed as MCP tools, and the design
  does not claim otherwise: a model that only speaks MCP tools
  cannot call it (§13a weighs this against Brave).
- Complete self-host specification (from SearXNG's official
  docs, 2026-10-17): container `searxng/searxng`, port 8080
  bound to loopback (or the LAN side, never public);
  `settings.yml` with `use_default_settings: true`,
  `server.secret_key` set, `server.limiter: true`, and
  `search.formats: [html, json]` — JSON is off by default and
  without it the API returns HTML. Verify with
  `curl "<instance>/search?q=test&format=json"`.
- SearXNG has no API key; access control is network placement
  plus the limiter. Cost is infrastructure only.
- BerriAI publishes a deployment helper
  (`github.com/BerriAI/serxng-deployment`, JSON pre-configured)
  as the reference for standing the container up.

### 6.4 Legacy servers

`github` and `gdrive` remain app-supervised Local servers in
v1. They are editor-oriented (their consumers are Continue/Cline
sessions), their credentials are file-shaped (a Drive
credentials path), and nothing in the council verdict requires
moving them. Registering them with the gateway later is a
per-server exposure flip once the registry exists — and for
`github`, the gateway path would also fix today's missing
credential injection (§1.4) as a side effect, since gateway
servers get real env resolution (§4.2).

## 7. Backend precondition: tool calls must actually fire

MCP tools reach a model only if the llama.cpp backend emits
structured `tool_calls`. Investigation C established the repo's
position precisely:

- The app **never sets `--jinja`** — the string occurs nowhere
  in the repo. Launch construction (`backend_spec`,
  `src-tauri/src/main.rs:1320`) sets only `-m`, `--port`, `-c`,
  `-b`, `--tensor-split` (when non-empty), and `--split-mode`,
  then appends `raw_flags`.
- The pinned llama.cpp build is b11658, whose server README
  documents `--jinja` as **default: enabled** — so tool calls
  plausibly work today by upstream default. But that default is
  invisible in this repo, version-dependent, and silently
  reversible by a `--no-jinja` typed into any backend's raw
  flags. A capability that depends on it must not.
- No chat-template handling exists anywhere: no schema field,
  no GGUF `chat_template` extraction, no `/props` call.
- At b11658, llama.cpp's native tool-call handlers list Qwen
  2.5 / 2.5-Coder (Hermes format). **Qwen3 and Qwen3-Coder are
  not in the native list** — Pete's two deployed models
  (planner: Qwen3.8-27B; coder: Qwen3-Coder-Next-80B) would run
  through a Hermes-compatible path or the Generic handler, and a
  documented Qwen3-Coder GGUF template bug (unguarded
  `properties|items` iteration → HTTP 500 on tools without
  `properties`) exists in the same model family. None of this
  is knowable from the repo; it is resolved by probing, below.

### 7.1 Launch change

- `backend_spec` adds `--jinja` to the structured flags for
  every backend. On the pinned build this is behaviour-
  preserving (it restates the upstream default) and makes the
  precondition explicit, audit-visible, and pin-independent.
- The raw-flags filter (today it strips `--host`,
  `src-tauri/src/main.rs:1414`) also strips `--no-jinja` in
  both spellings, with the same rationale class as `--host`:
  a reserved flag the app owns. An operator who truly needs it
  has lost nothing else — tool calling is the only consumer,
  and the probe (§7.2) would report the backend as failing.
- No `--chat-template`/`--chat-template-file` is set by
  default. If a probe shows the Generic handler or the template
  bug for a deployed model, the fix is a per-backend
  `raw_flags` template override — supported by today's
  pass-through — recorded as a catalog note, not new machinery.

### 7.2 The tool-call probe

New command `backend_test_tool_call(id)`, built on
`test_request`'s transport skeleton
(`crates/manager-supervisor/src/lib.rs:1485`) but a new function
— the existing body is hard-coded at `max_tokens: 16` and parses
only `content`:

1. Gate on `GET /health` (existing `HttpHealthProbe`).
2. `GET /props`; record whether `chat_template_tool_use` is
   present and the `chat_template_caps` summary. Absence is
   evidence, not yet failure.
3. POST `/v1/chat/completions` **direct to the backend**
   (loopback, bypassing the gateway, so llama.cpp behaviour is
   isolated from LiteLLM passthrough):
   - one probe tool with at least one defined property in
     `parameters.properties` (avoids the §7 template-bug class
     by construction);
   - `tool_choice: "required"`, a prompt that unambiguously
     needs the tool, `chat_template_kwargs:
     {enable_thinking: false}` (the wizard smoke test's
     precedent, `crates/manager-wizard/src/steps.rs:5776`),
     `max_tokens: 256`, 30 s timeout.
4. Pass = `choices[0].message.tool_calls` present with a name
   matching the probe tool and `arguments` that parse as JSON;
   `finish_reason` recorded either way. Text `<tool_call>…`
   leaking into `content` is a distinct recorded outcome
   ("unparsed"), not a pass and not a generic failure.

The result persists per backend in `AppConfig`
(`tool_call_probe: { outcome, at_ms }`, serde-defaulted) so it
survives restart, and renders on the backend card as
`Tool calls: verified | unparsed | failed | not measured`
(principle 12: absence of a probe is "not measured", never a
zero or a green). A backend restart, model change, or
raw-flags edit resets it to not measured, using the existing
drift machinery. Probing runs on demand from the card and the
MCP view; it is never run automatically against a metered
server — the probe is local-only and costs nothing but local
inference.

### 7.3 Gateway passthrough

A second, separate check repeats a minimal tools request
through the gateway per group (master key, one MCP tool from a
registered server, `require_approval` omitted so nothing
executes). It establishes that `tools`/`tool_calls` survive
LiteLLM unchanged for that group's backend — a runtime question
the repo cannot answer today (§14). It runs as part of the
on-machine validation phase (§12, phase 5), not at every start.

## 8. Protocol interop: the test is the check

LiteLLM negotiates the session-based MCP protocol at
`initialize`; the current spec revision (2026-07-28) removes
that handshake, and neither LiteLLM nor either candidate server
implements the stateless model per their docs (2026-10-17).
LiteLLM's exact MCP SDK pin for 1.104/1.105 is unverified, and
no pairwise test matrix exists. The design therefore does not
assert compatibility — it measures it, through the gateway
itself:

- `mcp_test_connection` is redefined for Gateway-exposed
  servers: with the master key it calls `GET /v1/mcp/server`
  (server registered?) and `GET /mcp-rest/tools/list?server_id=…`
  (tools returned?). A pass requires both — that is a real
  `initialize` + `tools/list` round trip against the actual
  server, through the actual gateway build, which is precisely
  the interop surface in question.
- The same two calls run at the end of every start-gateway run
  that registered MCP servers; a server that fails is marked
  failed on its card with the gateway's error text, and the
  gateway start itself still succeeds (a broken MCP server must
  not take model routing down — §10.2).
- An explicit per-server **Test** action additionally performs
  one real tool call for curated servers with fixed, harmless
  arguments (Brave: a one-word query; fetch: never automatic —
  §5.2). Metered calls happen only from this explicit action,
  never from a poll or a start.
- For Local (legacy) servers the test stays what it is today —
  process liveness / HTTP probe — and the UI labels it
  accurately as such. Giving Local servers a real protocol test
  requires an MCP client in the app, which this design does not
  build; the spec's old "tool-list probe" promise
  (`docs/technical-specification.md:528`) is corrected by this
  design rather than repeated.

## 9. Manager UI

One new view, following `docs/design-principles.md`. MCP is its
own question ("what tools can clients call, and are they
live?"), distinct from Gateway routing, so it gets its own
route rather than lengthening the Gateway view (principles 1, 4):

- Route `#/mcp`, label "MCP Servers", placed after Gateway in
  the nav (the Optimize placement pattern).
- Top strip (one line, Label: Value): `LiteLLM: <version>`,
  `Stdio: enabled | disabled`, `Registered: <n>` — each value
  in exactly one place (principle 7); the version is the §4.3
  probe's, or "not measured".
- One **server card** per registry entry. Card top is the
  answer: name + status chip (green registered, gray disabled,
  light red failed, amber transitional — principle 11; a chip
  exists only for non-default state). Lines, all Label: Value:
  `Transport:`, `Command:` or `URL:` (copy button for URLs,
  principle 10), `Credential: present | missing`, `Tools:
  <count>` when measured (omitted when not — principles 12, 13),
  and for metered servers `Cost: $5 per 1,000 requests` and
  `Rate cap: 20/min`. Explanation lives in tooltips
  (principle 5).
- Card actions, per principle 15: **Test** is the card's
  standard workflow action — words + icon. Enable/disable is a
  toggle (icon-only with aria-label). Edit is icon-only.
  **Remove** is destructive — words + icon, with confirmation.
  Storing a credential follows the existing pattern (a Store
  action that writes to Credential Manager; the value is never
  displayed after entry).
- **Add server** opens a form card: id (validated live against
  §3.2), transport choice, command/args or URL, env key names +
  credential target, exposure choice. The command field's
  tooltip lists the allowlist. Save goes through the normal
  config save, so the change appears in the Activation Plan
  like any other (§10.2).
- Backend cards gain the §7.2 `Tool calls:` line. The MCP view
  shows an aggregate warning only when zero enabled backends
  are verified — a state banner, not a duplicated count.
- Wizard: the `mcp-servers` step is reworked to check
  credential presence for every enabled registry server that
  names a `cred_ref` (not just the two legacy ones), and stops
  writing `mcp-servers.json` (§10.1).

`ui/src/api.ts` gains wrappers for the reworked commands:
`mcp_list` (extended payload), `mcp_test_connection`
(redefined, §8), plus new `mcp_save_server`, `mcp_delete_server`,
and `mcp_set_exposure` replacing `mcp_set_mode`.

## 10. Migration and rollback

### 10.1 Migration (one-time, on first run of the new build)

- `AppConfig.mcp.servers` is seeded from today's
  `default_servers()` — `github` and `gdrive` as Local entries —
  so behaviour for existing installs is unchanged on day one.
- Credential namespace is unified on the wizard's prefix,
  `local-llm-service-manager/…` (the targets the wizard step
  actually checks, and the naming the rest of the app uses for
  the master key and Postgres). Migration sets `cred_ref` to
  the wizard target; presence checks accept **either** target
  during a transition release, so a credential stored under the
  registry's `llm-manager/mcp/…` prefix keeps working and is
  reported under its found name.
- The Drive env key is canonicalised to
  `GOOGLE_APPLICATION_CREDENTIALS` (what the server reads; the
  registry's `GDRIVE_CREDENTIALS_PATH` was the divergent one).
- The session-only `mcp_modes` overlay is deleted; persisted
  exposure replaces it.
- `<data_dir>/mcp/mcp-servers.json` is retired as a store: the
  wizard step stops writing it. The file is left in place
  (nothing reads it; deleting user files is not this
  migration's job) and noted as inert in the step's log line.
- `editor_mcp_config()` stays as-is (dead code, Continue/Cline
  shape). Editors are directed at the gateway `/mcp` endpoint
  instead (§1.3); reviving editor-file delivery is a separate
  decision if Pete wants it.

### 10.2 Rollback and the bad-config scenario

The council's scenario: a bad MCP definition reaches the
gateway and LiteLLM refuses to start, taking model routing down
with it. Defences, in the order they fire:

1. **Save-time validation** (§3.2) — the classes LiteLLM
   rejects at startup (hyphenated names, non-allowlisted
   commands) cannot be stored.
2. **Generation-time failure** — `generate_config_yaml` fails
   closed on invalid input before any file is written, the
   generator's existing pattern (unknown member, empty
   allowlist).
3. **Start-time floor + post-start check** — the §4.3 version
   floor fails the step before spawn; the §8 load check runs
   after spawn and degrades per-server, never the gateway.
4. **Cancel restores the YAML too.** Today, activation cancel
   restores the pre-change `AppConfig` snapshot but the
   generated `config.yaml` on disk is only rewritten at the
   next start (Investigation B, §5). This design closes that
   hole for all gateway changes: cancel regenerates
   `config.yaml` from the restored config immediately, so disk,
   config, and the running gateway agree again. The journal
   also records the pre-change YAML hash for drift visibility.

Residual, stated plainly: a YAML that parses and passes the
gates but that LiteLLM rejects semantically at load would still
surface only through the existing `wait_healthy` failure — at
which point the old gateway is already stopped. No LiteLLM
dry-run or shadow-start mode is documented (2026-10-17), so
gate 4 is the recovery path rather than a prevention. The
gates make the known rejection classes unreachable; the
residual is the unknown ones, and the recovery is one cancel.

Feature-level rollback is trivial by construction: set every
server's exposure to Disabled (or delete the curated entries)
and apply — the generator emits no `mcp_servers:` block, the
spawn env loses the stdio flag, and the system is byte-identical
to pre-feature behaviour.

## 11. Operations

- **Health.** Per-server state comes from `GET /v1/mcp/server`
  at gateway start and on MCP view load (§8). LiteLLM logs a
  warning naming each stdio server it will not start; that
  cannot happen by construction here (the manager sets the
  flag when stdio servers exist), and if a future LiteLLM
  changes the semantics, the load check shows the server as
  failed rather than silently absent.
- **Logs.** MCP activity lands in the gateway's existing
  stdout/stderr logs (`<data_dir>/logs/gateway.*.log`), which
  the Logs view already reads. No new log store. Stdio server
  stderr is part of the gateway's stderr by LiteLLM's process
  model.
- **Cost visibility.** §6.1: rpm cap and per-key spend in the
  UI; Brave credit balance is dashboard-only and renders as not
  measured. Search queries leave the machine for Brave — the
  server card's `Provider: Brave (paid API)` line makes that
  data flow visible where the server is managed, per the
  council's data-exposure note.
- **Failure modes.**

| Failure | Effect | Surface |
|---|---|---|
| Brave credit exhausted / API down | Tool errors to the calling client only | Server card test shows the API message |
| MCP server process crashes | LiteLLM marks the server unhealthy; routing unaffected | Card status on next load/test |
| Credential deleted from Credential Manager | Server omitted at next gateway start, warning logged | Card: `Credential: missing` |
| LiteLLM below 1.83.7 with stdio servers | Gateway start fails closed with remediation | Step log (§4.3) |
| Backend cannot emit tool calls | Tools exist but never fire for that backend | Backend card: `Tool calls: failed` (§7.2) |
| Bad MCP definition saved | Rejected at save or generation | Form/plan error (§10.2) |

- **Capacity.** Gateway-registered stdio servers are one
  process per server per gateway (LiteLLM's model), not one per
  client session — the blueprint's per-session process concern
  does not apply. Two small Node/Python processes on the
  workstation are within the noise of one llama-server.

## 12. Implementation sketch

### New and changed code

| Area | Change |
|---|---|
| `crates/manager-config` | `McpConfig`/`McpServerConfig` (§3.1), validation (§3.2), `tool_call_probe` field on backends; defaults seed the curated servers |
| `crates/manager-mcp` | Curated defaults move to the persisted registry; `ServerSpec` becomes the runtime view of `McpServerConfig`; legacy Local spawn path unchanged |
| `crates/manager-gateway` | `mcp_servers:` emission (§4.1) + snapshot tests; fixture unchanged when no gateway servers |
| `crates/manager-wizard` | Spawn env: stdio flag + per-server secret resolution (§4.2); `litellm_version_command` + floor (§4.3); fingerprint fields; manual-start block; `mcp-servers` step rework (§9) |
| `crates/manager-supervisor` | Tool-call probe function beside `test_request` (§7.2) with fake-server tests |
| `src-tauri` | `backend_spec`: `--jinja` + `--no-jinja` filter (§7.1); commands: extended `mcp_list`, redefined `mcp_test_connection`, `mcp_save_server`, `mcp_delete_server`, `mcp_set_exposure`, `backend_test_tool_call`; activation cancel regenerates YAML (§10.2) |
| `ui` | `#/mcp` view + server cards + add form; `api.ts` wrappers; backend card `Tool calls:` line; nav entry |

Core logic placement follows the repo's existing split: schema
and validation in `manager-config` (Linux-CI-testable), YAML
generation in `manager-gateway`, protocol probes in
`manager-supervisor`/`manager-wizard`; `src-tauri` holds glue.

### Phases

0. **Groundwork probes.** LiteLLM version probe + floor check;
   `--jinja` explicit + `--no-jinja` filter; `backend_test_tool_call`
   command (no UI). This phase resolves the two load-bearing
   unknowns on Pete's machine before any MCP surface is built.
1. **Registry.** `AppConfig.mcp` schema + validation + migration
   (§10.1); commands (save/delete/exposure); overlay removal.
2. **Gateway integration.** `mcp_servers:` generation; spawn-env
   changes; fingerprint; post-start load check; cancel
   regenerates YAML.
3. **Curated servers.** `brave_search` + `web_fetch` seeding
   with pinned versions, tool trimming, rpm caps; SearXNG
   `search_tools:` wiring behind its own toggle.
4. **UI.** `#/mcp` view, wizard step rework, backend card
   tool-call line, `api.ts` wrappers.
5. **On-machine validation.** Per-model probes for planner,
   coder, coder-fast, tool-runner; gateway passthrough check
   (§7.3); Brave plan confirmation in the dashboard; interop
   records from §8's test for both curated servers.

## 13. Decisions for Pete

### (a) Search provider for v1

- **Brave only (recommended).** A maintained official MCP
  server with a purpose-built grounding tool
  (`brave_llm_context`); models can call it as a tool today.
  Cost: $5 per 1,000 requests, $5 monthly credit, card
  required; bounded by §5.5.
- **SearXNG only.** No per-query cost and no data leaves for a
  third-party search API, but: a container to stand up and
  operate (§6.3), and it reaches clients only through LiteLLM's
  Search API endpoint — models speaking MCP tools cannot call
  it, so the "search as a tool" capability does not exist in
  this option.
- **Both.** Brave as the tool surface now; SearXNG added in
  phase 3 as the Search-API provider for clients that use it.
  More surface to validate; nothing is wasted.

Recommendation: Brave only for v1; add SearXNG when a client
actually needs the Search API or if Brave's pricing changes the
tradeoff.

### (b) Tool-execution gating default

- **Explicit grants (recommended).** `require_key_mcp_access_defined:
  true` plus per-key `object_permission` grants (§5.3). A key
  gets MCP tools only when the operator grants them. Direct
  `/mcp` clients still approve individual calls in their own
  UI; chat clients passing `require_approval: "never"` get
  server-side execution only on granted keys.
- **Open (LiteLLM default).** Every key holder can reach every
  registered server. Simpler; on a single-user LAN appliance
  the exposure is bounded by who holds a key — but the metered
  Brave key is then spendable by any client that holds any
  gateway key, with prompt injection as the trigger (§5.2).

Recommendation: explicit grants. The cost is one checkbox per
key; the alternative makes a paid, injection-reachable tool
surface the default for every future key.

### (c) Consumer surfaces for v1

- **Gateway surfaces only (recommended):** external MCP
  clients via `/mcp`, LLM clients via chat/Responses (§1.3).
  Editors register the gateway endpoint in their own MCP
  settings — one server set for every client, managed in one
  place.
- **Plus editor-file delivery:** revive `editor_mcp_config()`
  and write per-editor MCP files (Continue/Cline) listing the
  servers for direct (non-gateway) spawning. This duplicates
  the registry into files that drift, and reintroduces
  per-editor server processes; it is what the current dead code
  gestures at.

Recommendation: gateway surfaces only. Editor files can be
revisited if an editor Pete uses cannot consume a remote MCP
endpoint.

### (d) Fetch in v1

- **Include, disabled by default (recommended).** The registry,
  validation (including the banned `--allow-private-ips`), and
  card ship in v1; the server is enabled only by an explicit
  toggle. Search-only use never turns it on.
- **Exclude from v1.** Simpler launch; fetch arrives later
  with its own review. The cost is a second registry/catalog
  change for a server whose controls are already fully
  specified in §5.2.

Recommendation: include, disabled by default.

## 14. Residual unknowns (need Pete's machine)

These are not answerable from the repo or the web, and the
phases above are ordered to resolve them before they can hurt:

1. **Installed LiteLLM version.** The repo expects 1.105.1
   (field-verified comments) but does not pin it. Phase 0's
   probe reads the real version; the §4.3 floor acts on it.
2. **Per-model tool-call results.** Whether the planner
   (Qwen3.8-27B, ISTA-DASLab GGUF — template uninspected) and
   the coder (Qwen3-Coder-Next-80B, Unsloth GGUF — documented
   template-bug class in the family) emit structured
   `tool_calls` at b11658, and through which handler. Resolved
   by §7.2 probes in phase 0/5; coder-fast and tool-runner
   (Qwen 2.5-Coder / Qwen3-8B class) are the expected control
   group.
3. **Gateway passthrough.** Whether LiteLLM forwards `tools`
   and returns `tool_calls` unchanged for an `openai/`
   llama-server deployment. Resolved by §7.3 in phase 5.
4. **Brave account facts.** Which plan the key is on, its real
   rate limit, and whether the $5 credit is active — dashboard
   only (§6.1).
5. **Credential namespace on Pete's machine.** Whether his
   existing GitHub/Drive credentials sit under the wizard's
   targets or the registry's (§10.1 handles both, but the
   migration report should say which it found).
6. **Database-registered MCP servers.** Whether LiteLLM, when
   running with `DATABASE_URL` set as this app runs it, also
   surfaces admin-UI-registered MCP servers next to config-file
   ones. If it does, servers added through LiteLLM's own UI
   would exist outside the manager's source of truth; phase 2
   validation should check `GET /v1/mcp/server` against the
   generated set and report any extras.
7. **LiteLLM's exact MCP SDK pin at 1.104/1.105.** Informational
   only — §8's empirical test is the operative gate.

## Sources

Repo evidence: Investigations A–C, 2026-10-17 (file:line
citations inline above). Web evidence, all accessed 2026-10-17:
LiteLLM MCP docs (`docs.litellm.ai/docs/mcp`,
`/mcp_deployment`, `/mcp_config_reference`, `/mcp_control`,
`/mcp_usage`, `/mcp_public_internet`), BerriAI security post on
CVE-2026-30623 (2026-04-21), LiteLLM SearXNG search docs,
Brave MCP server repo (`github.com/brave/brave-search-mcp-server`),
`mcp-server-fetch` README (PyPI), SearXNG admin docs, llama.cpp
b11658 `docs/function-calling.md` and server README.

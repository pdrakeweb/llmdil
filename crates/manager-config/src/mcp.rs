//! MCP server registry config: transports, credential references,
//! the curated/seeded server lists, SearXNG settings, and the
//! save-time MCP validation rules (MCP design §3.2 / §5.1).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// MCP transport, as persisted in [`McpServerConfig`].
///
/// Defined here, in the schema crate, rather than reusing
/// `manager_mcp::McpTransport`: `manager-config` is a leaf crate and
/// `manager-mcp` already depends on the leaf supervisor crate — having
/// config depend on `manager-mcp` would invert that layering. The serde
/// shape is identical to the `manager-mcp` enum (lowercase `"stdio"` /
/// `"http"`), and `manager-mcp` converts between the two (it may depend
/// on this leaf crate without a cycle). This is a deliberate
/// design-vs-code discrepancy from design §3.1, which assumed the
/// existing `manager-mcp` enum could be reused directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
}

/// Where an MCP server is exposed (MCP design §3.1). Replaces the
/// session-only `McpMode` overlay for persistence. Serde naming follows
/// this crate's convention for mode enums (`lowercase`, as on
/// [`ViewMode`](crate::ViewMode)/[`MxcMode`](crate::MxcMode)): `"gateway"` / `"local"` / `"disabled"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum McpExposure {
    /// Registered in the generated LiteLLM gateway config.
    Gateway,
    /// App-supervised local process (the legacy github/gdrive behaviour).
    Local,
    /// Not registered and not spawned.
    #[default]
    Disabled,
}

/// Credential Manager target for the legacy GitHub MCP server, in the
/// unified namespace the wizard uses (MCP design §10.1). The same
/// string is `GITHUB_CRED_TARGET` in `manager-wizard`; it is defined
/// here because the registry seed lives in this leaf crate and the
/// wizard depends on config, not the reverse.
pub const GITHUB_MCP_CRED_REF: &str = "local-llm-service-manager/github-mcp";
/// Credential Manager target for the legacy Google Drive MCP server,
/// unified namespace (see [`GITHUB_MCP_CRED_REF`]).
pub const GDRIVE_MCP_CRED_REF: &str = "local-llm-service-manager/gdrive-mcp";
/// Pre-unification credential target for the GitHub server
/// (`llm-manager/mcp/github`, from `manager_mcp::default_servers`).
/// Presence checks accept either target during the transition
/// (MCP design §10.1).
pub const LEGACY_GITHUB_MCP_CRED_REF: &str = "llm-manager/mcp/github";
/// Pre-unification credential target for the Drive server.
pub const LEGACY_GDRIVE_MCP_CRED_REF: &str = "llm-manager/mcp/gdrive";
/// Credential Manager target for the curated Brave Search MCP server
/// (MCP design §3.3).
pub const BRAVE_SEARCH_MCP_CRED_REF: &str = "local-llm-service-manager/brave-search-mcp";

/// Pinned Brave Search MCP server package (MCP design §3.3 / §6.1).
///
/// Pin: `@brave/brave-search-mcp-server@2.1.4` — the npm registry's
/// `dist-tags.latest` for `@brave/brave-search-mcp-server`, resolved
/// 2026-10-17. Recorded in the design's §3.3 implementation record.
/// Bumping it is a code change, never a runtime resolution.
pub const BRAVE_SEARCH_PACKAGE_PIN: &str = "@brave/brave-search-mcp-server@2.1.4";
/// Where a user registers for a Brave Search API key and copies
/// one: the page the start-gateway credential prompt links to
/// (Pete, 2026-10-18). Carried on the curated seed entry as
/// [`McpServerConfig::signup_url`] and surfaced via `mcp_list` and
/// the start preflight — operator-facing metadata only, never
/// emitted into the generated LiteLLM YAML.
pub const BRAVE_SEARCH_SIGNUP_URL: &str = "https://brave.com/search/api/";
/// Pinned URL-fetch MCP server package (MCP design §3.3 / §6.2).
///
/// Pin: `mcp-server-fetch@2026.10.10` — the PyPI latest release of
/// the date-versioned `mcp-server-fetch` project, resolved
/// 2026-10-17. Recorded in the design's §3.3 implementation record.
pub const WEB_FETCH_PACKAGE_PIN: &str = "mcp-server-fetch@2026.10.10";
/// The Brave tools exposed by default (MCP design §3.3): the other
/// six Brave tools stay off until the operator edits the server.
/// Emitted both as LiteLLM `allowed_tools` and as the server's own
/// `BRAVE_MCP_ENABLED_TOOLS` trimming env (by `manager-gateway`).
pub const BRAVE_DEFAULT_ALLOWED_TOOLS: &[&str] = &["brave_web_search", "brave_llm_context"];

/// SearXNG search-provider settings (MCP design §6.3): SearXNG is
/// NOT an MCP server — LiteLLM ships it as a built-in search
/// provider, reached through its Search API. These settings live on
/// [`McpConfig`] because §6.3 specifies the feature inside the MCP
/// capabilities design and the registry section is where its toggle
/// belongs in the persisted shape; generation emits a top-level
/// `search_tools:` block (never inside `mcp_servers:`) and the
/// gateway spawn env gains `SEARXNG_API_BASE`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearxngSettings {
    /// Operator toggle; off by default (no instance is stood up by
    /// the app — §6.3's container is the operator's to run).
    pub enabled: bool,
    /// Base URL of the operator's SearXNG instance, e.g.
    /// `http://127.0.0.1:8080`. Not a secret: it is plain config,
    /// shown in the gateway plan like other non-secret values.
    /// Validated as a non-empty http(s) URL only while `enabled`.
    pub api_base: String,
}

/// The persisted MCP server registry (MCP design §3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    /// SearXNG search-provider settings (§6.3). Serde-defaulted so
    /// registry JSON written before the settings existed loads with
    /// SearXNG disabled and no instance URL.
    #[serde(default)]
    pub searxng: SearxngSettings,
}

impl McpConfig {
    /// The seeded registry: the two legacy servers from
    /// `manager_mcp::default_servers()`, as `Local` entries, but with
    /// the unified credential namespace and the Drive env key
    /// canonicalised to `GOOGLE_APPLICATION_CREDENTIALS` (MCP design
    /// §10.1; the legacy registry's `GDRIVE_CREDENTIALS_PATH` was the
    /// divergent name — the Drive server reads
    /// `GOOGLE_APPLICATION_CREDENTIALS`, which is also what the wizard
    /// step always wrote). Command/args/package parity with
    /// `default_servers()` is otherwise exact: `npx -y` on the same
    /// `@modelcontextprotocol/server-*` packages. Phase 3 appends the
    /// two curated servers ([`curated_mcp_servers`]) after the legacy
    /// pair, so a fresh registry carries all four entries.
    pub fn seeded() -> Self {
        Self {
            servers: seeded_mcp_servers(),
            searxng: SearxngSettings::default(),
        }
    }
}

/// `Default` for [`McpConfig`] is the *seeded* registry, not an empty
/// one: `AppConfig`'s `#[serde(default)] mcp` field uses this, so a
/// config file with no `mcp` key at all loads with the legacy servers
/// present (the §10.1 migration for pre-feature configs). An explicit
/// empty registry is written as `McpConfig { servers: Vec::new() }`
/// and is preserved as empty — serde only invokes this default when
/// the key is absent, and [`migrate`](crate::migrate()) preserves an explicit section
/// verbatim.
impl Default for McpConfig {
    fn default() -> Self {
        Self::seeded()
    }
}

/// The curated Phase 3 seed servers (MCP design §3.3): Brave Search
/// (Gateway-exposed, metered) and URL fetch (Disabled by default,
/// §13d). Pins are the [`BRAVE_SEARCH_PACKAGE_PIN`] /
/// [`WEB_FETCH_PACKAGE_PIN`] constants — exact versions resolved
/// from the primary registries (npm / PyPI) on 2026-10-17 and
/// recorded in the design's §3.3 implementation record.
pub fn curated_mcp_servers() -> Vec<McpServerConfig> {
    vec![
        McpServerConfig {
            id: "brave_search".to_string(),
            display_name: "Brave Search".to_string(),
            transport: McpTransport::Stdio,
            exposure: McpExposure::Gateway,
            command: Some("npx".to_string()),
            args: vec![
                "-y".to_string(),
                BRAVE_SEARCH_PACKAGE_PIN.to_string(),
                "--transport".to_string(),
                "stdio".to_string(),
            ],
            url: None,
            env_keys: vec!["BRAVE_API_KEY".to_string()],
            cred_ref: Some(BRAVE_SEARCH_MCP_CRED_REF.to_string()),
            credential_hint: Some(
                "Brave Search API key — the subscription key from the Brave Search API page"
                    .to_string(),
            ),
            allowed_tools: Some(
                BRAVE_DEFAULT_ALLOWED_TOOLS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            signup_url: Some(BRAVE_SEARCH_SIGNUP_URL.to_string()),
            metered: true,
            // Stored on the entry and surfaced via `mcp_list` only.
            // It is deliberately NOT emitted into the generated
            // LiteLLM YAML: LiteLLM's `mcp_rpm_limit` is a key/team
            // property, not a server-config property, and emitting
            // an invented server key risks a LiteLLM config
            // rejection. Actual enforcement attaches at virtual-key
            // issuance (design §5.5) — which this app does not
            // perform (there is no virtual-key issuance surface;
            // the Phase 2 discrepancy recorded in
            // docs/mcp-capabilities-design.md §12 / the Phase 2
            // implementation), so in v1 the value is the documented
            // cap the operator sees, not a gateway-enforced limit.
            rpm_limit: Some(20),
        },
        McpServerConfig {
            id: "web_fetch".to_string(),
            // URL fetching only — never described as search (§6.2).
            display_name: "Web Fetch".to_string(),
            transport: McpTransport::Stdio,
            // Disabled until explicitly enabled (§5.2 / §13d).
            exposure: McpExposure::Disabled,
            command: Some("uvx".to_string()),
            args: vec![WEB_FETCH_PACKAGE_PIN.to_string()],
            url: None,
            env_keys: Vec::new(),
            cred_ref: None,
            // `fetch` is the server's only tool (§6.2).
            allowed_tools: Some(vec!["fetch".to_string()]),
            metered: false,
            rpm_limit: None,
            signup_url: None,
            credential_hint: None,
        },
    ]
}

/// The seed servers for [`McpConfig::seeded`] (see its documentation
/// for the unification/canonicalisation rules applied): the two
/// legacy Local servers followed by the Phase 3 curated pair
/// ([`curated_mcp_servers`]).
pub fn seeded_mcp_servers() -> Vec<McpServerConfig> {
    let mut servers = vec![
        McpServerConfig {
            id: "github".to_string(),
            display_name: "GitHub".to_string(),
            transport: McpTransport::Stdio,
            exposure: McpExposure::Local,
            command: Some("npx".to_string()),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-github".to_string(),
            ],
            url: None,
            env_keys: vec!["GITHUB_PERSONAL_ACCESS_TOKEN".to_string()],
            cred_ref: Some(GITHUB_MCP_CRED_REF.to_string()),
            credential_hint: Some(
                "GitHub personal access token — created under GitHub Settings, Developer settings, Personal access tokens"
                    .to_string(),
            ),
            allowed_tools: None,
            metered: false,
            rpm_limit: None,
            signup_url: None,
        },
        McpServerConfig {
            id: "gdrive".to_string(),
            display_name: "Google Drive".to_string(),
            transport: McpTransport::Stdio,
            exposure: McpExposure::Local,
            command: Some("npx".to_string()),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-gdrive".to_string(),
            ],
            url: None,
            // Canonicalised (design §10.1): not the legacy
            // GDRIVE_CREDENTIALS_PATH.
            env_keys: vec!["GOOGLE_APPLICATION_CREDENTIALS".to_string()],
            cred_ref: Some(GDRIVE_MCP_CRED_REF.to_string()),
            // File-shaped credential (design §6.4): the stored
            // value is the credentials file's path, not its
            // contents — the server reads the env var as a path.
            credential_hint: Some(
                "Path to the Google OAuth credentials JSON file — downloaded from Google Cloud Console, APIs & Services, Credentials; store the file path, not the file contents"
                    .to_string(),
            ),
            allowed_tools: None,
            metered: false,
            rpm_limit: None,
            signup_url: None,
        },
    ];
    servers.extend(curated_mcp_servers());
    servers
}

/// One persisted MCP server definition (MCP design §3.1). Every field
/// carries a serde default so a partial JSON object loads: missing
/// strings are empty, missing lists are empty, missing options are
/// `None`, `transport` defaults to `stdio`, and `exposure` defaults
/// to `disabled` (the safe choice for a partially-specified server —
/// it is neither registered with the gateway nor spawned).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub id: String,
    pub display_name: String,
    pub transport: McpTransport,
    pub exposure: McpExposure,
    /// Stdio command; allowlist-validated at save (§3.2).
    pub command: Option<String>,
    /// Stdio args; pinned package versions for curated servers.
    pub args: Vec<String>,
    /// HTTP transport base URL.
    pub url: Option<String>,
    /// Env var NAMES the server needs; never values.
    pub env_keys: Vec<String>,
    /// Credential Manager target name for the `env_keys` values.
    pub cred_ref: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    /// Paid upstream; drives the rpm cap + UI cost labelling.
    pub metered: bool,
    pub rpm_limit: Option<u32>,
    /// Where the user obtains the credential (the service's
    /// registration / API-key page), when the server needs one:
    /// the start-gateway credential prompt links there, and the
    /// MCP Servers view shows the same link while the credential
    /// is missing. Curated seed metadata; `None` for servers
    /// without a public signup page. Never emitted into the
    /// generated gateway config.
    pub signup_url: Option<String>,
    /// What credential the operator stores for this server — one
    /// short instruction naming the credential kind and where it
    /// comes from (e.g. "Brave Search API key — the subscription
    /// key from the Brave Search API page"). The MCP Servers view
    /// renders it as the credential field's label and the
    /// start-gateway prompt shows it beside the store field, so
    /// the operator is never asked for an unidentified value
    /// (Pete, 2026-10-18). Operator-facing metadata like
    /// `signup_url`: surfaced via `mcp_list` (resolved against the
    /// seed for entries persisted before the field existed) and
    /// never emitted into the generated gateway config. `None`
    /// for servers without a credential, and for custom servers
    /// whose operator has not described one — the view then
    /// labels the field from the env key names instead.
    pub credential_hint: Option<String>,
}

/// Stdio commands LiteLLM allows for MCP servers; the manager mirrors
/// the list at save time (MCP design §3.2 / §5.1). Public because the
/// app's local-process spawn path re-checks it at spawn time (review
/// v0.1.60 PS-5).
pub const MCP_STDIO_COMMAND_ALLOWLIST: &[&str] =
    &["npx", "uvx", "python", "python3", "node", "docker", "deno"];

/// Banned MCP server args (MCP design §3.2): rejected at save, never
/// stripped, so the operator sees the refusal. Public for the same
/// spawn-time re-check as [`MCP_STDIO_COMMAND_ALLOWLIST`].
pub const MCP_BANNED_ARGS: &[&str] = &["--allow-private-ips", "--ignore-robots-txt"];

/// The banned token `arg` matches, if any: the token itself or its
/// `token=value` form (MCP design §3.2). Shared by save-time
/// validation and the spawn-time re-check.
pub fn mcp_banned_arg(arg: &str) -> Option<&'static str> {
    MCP_BANNED_ARGS.iter().copied().find(|banned| {
        arg == *banned
            || arg
                .strip_prefix(banned)
                .is_some_and(|rest| rest.starts_with('='))
    })
}

/// True when `command`'s basename (see [`mcp_command_basename`]) is in
/// [`MCP_STDIO_COMMAND_ALLOWLIST`].
pub fn mcp_stdio_command_allowed(command: &str) -> bool {
    MCP_STDIO_COMMAND_ALLOWLIST.contains(&mcp_command_basename(command).as_str())
}

/// True when `id` matches `^[a-z][a-z0-9_]*$` (hand-rolled: this crate
/// has no regex dependency, and the alphabet is small).
fn is_valid_mcp_server_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The basename used for the stdio allowlist check: the last path
/// component, splitting on BOTH `/` and `\` (a persisted config may
/// carry a Windows-style path such as `C:\tools\npx.cmd` regardless of
/// the OS reading it), lowercased, with a single trailing executable
/// extension (`.exe` / `.cmd` / `.bat`) removed. So `npx`,
/// `C:\tools\npx.cmd`, `/usr/local/bin/NPX.EXE`, and `npx.exe` all
/// normalise to `npx`.
pub fn mcp_command_basename(command: &str) -> String {
    let base = command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command)
        .to_lowercase();
    for ext in [".exe", ".cmd", ".bat"] {
        if let Some(stripped) = base.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    base
}

/// Pragmatic HTTP(S) URL check for MCP http servers (this crate has
/// no `url` dependency; the existing style is hand-rolled checks like
/// the gateway `bind_host` IP parse). Returns the lowercase host on
/// success. Rejects: no `://`, a scheme other than http/https, an
/// empty host, whitespace/control characters, and a non-numeric port.
fn parse_mcp_http_url(url: &str) -> Result<(String, String), String> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("url must not contain whitespace or control characters".to_string());
    }
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| "url must be of the form <scheme>://<host>[/...]".to_string())?;
    let scheme = scheme.to_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!("url scheme {scheme:?} is not http or https"));
    }
    // Authority ends at the first path/query/fragment delimiter.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Strip any userinfo.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return Err("url has no host".to_string());
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        // IPv6 literal: [::1] or [::1]:port
        let (h, tail) = bracketed
            .split_once(']')
            .ok_or_else(|| "url has an unterminated IPv6 host".to_string())?;
        if let Some(port) = tail.strip_prefix(':') {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("url port {port:?} is not numeric"));
            }
        } else if !tail.is_empty() {
            return Err("url has unexpected characters after the IPv6 host".to_string());
        }
        h.to_lowercase()
    } else {
        match authority.rsplit_once(':') {
            Some((h, port)) => {
                if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(format!("url port {port:?} is not numeric"));
                }
                h.to_lowercase()
            }
            None => authority.to_lowercase(),
        }
    };
    if host.is_empty() {
        return Err("url has no host".to_string());
    }
    Ok((scheme, host))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// MCP registry validation (MCP design §3.2), called from [`validate`].
pub(crate) fn validate_mcp_servers(mcp: &McpConfig, errors: &mut Vec<String>) {
    let mut ids: HashSet<String> = HashSet::new();
    for s in &mcp.servers {
        if !is_valid_mcp_server_id(&s.id) {
            errors.push(format!(
                "mcp server id {:?} must match ^[a-z][a-z0-9_]*$",
                s.id
            ));
        } else if !ids.insert(s.id.clone()) {
            errors.push(format!("duplicate mcp server id {:?}", s.id));
        }
        for arg in &s.args {
            if let Some(banned) = mcp_banned_arg(arg) {
                errors.push(format!(
                    "mcp server {:?}: arg {banned:?} is banned and is rejected, not stripped",
                    s.id
                ));
            }
        }
        match s.transport {
            McpTransport::Stdio => match s.command.as_deref().map(str::trim) {
                Some(cmd) if !cmd.is_empty() => {
                    let base = mcp_command_basename(cmd);
                    if !MCP_STDIO_COMMAND_ALLOWLIST.contains(&base.as_str()) {
                        errors.push(format!(
                                "mcp server {:?}: command basename {base:?} is not in the stdio allowlist {:?}",
                                s.id, MCP_STDIO_COMMAND_ALLOWLIST
                            ));
                    }
                }
                _ => errors.push(format!(
                    "mcp server {:?}: stdio transport requires a command",
                    s.id
                )),
            },
            McpTransport::Http => match s.url.as_deref().map(str::trim) {
                Some(url) if !url.is_empty() => match parse_mcp_http_url(url) {
                    Ok((scheme, host)) => {
                        if scheme == "http" && !is_loopback_host(&host) {
                            errors.push(format!(
                                    "mcp server {:?}: http url is only allowed on loopback (127.0.0.1 / localhost / ::1); use https off-loopback",
                                    s.id
                                ));
                        }
                    }
                    Err(e) => errors.push(format!("mcp server {:?}: {e}", s.id)),
                },
                _ => errors.push(format!(
                    "mcp server {:?}: http transport requires a url",
                    s.id
                )),
            },
        }
        if s.exposure == McpExposure::Gateway
            && !s.env_keys.is_empty()
            && s.cred_ref
                .as_deref()
                .map(str::trim)
                .map(|r| r.is_empty())
                .unwrap_or(true)
        {
            errors.push(format!(
                "mcp server {:?}: gateway-exposed server with env_keys must name a cred_ref (the credential itself may be missing — that is a runtime state, not a save error)",
                s.id
            ));
        }
        if let Some(url) = s
            .signup_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        {
            if let Err(e) = parse_mcp_http_url(url) {
                errors.push(format!("mcp server {:?}: signup_url: {e}", s.id));
            }
        }
    }
}

/// SearXNG settings validation (MCP design §6.3): only the enabled
/// state constrains anything — `api_base` must then be a non-empty
/// http(s) URL (checked with the same pragmatic parser as MCP http
/// servers). Unlike MCP http servers there is no https-off-loopback
/// rule: a SearXNG instance is operator infrastructure, bound to
/// loopback or the LAN side per §6.3, so plain http to a LAN address
/// is a legitimate configuration. Disabled carries no constraint at
/// all, so a half-typed URL can be saved with the toggle off.
pub(crate) fn validate_searxng(searxng: &SearxngSettings, errors: &mut Vec<String>) {
    if !searxng.enabled {
        return;
    }
    let api_base = searxng.api_base.trim();
    if api_base.is_empty() {
        errors.push("mcp.searxng: api_base must not be empty while searxng is enabled".to_string());
        return;
    }
    if let Err(e) = parse_mcp_http_url(api_base) {
        errors.push(format!("mcp.searxng: api_base: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::*;
    use crate::*;

    fn mcp_server(id: &str) -> McpServerConfig {
        McpServerConfig {
            id: id.to_string(),
            display_name: id.to_string(),
            transport: McpTransport::Stdio,
            exposure: McpExposure::Local,
            command: Some("npx".to_string()),
            args: vec!["-y".to_string(), "pkg".to_string()],
            url: None,
            env_keys: Vec::new(),
            cred_ref: None,
            signup_url: None,
            credential_hint: None,
            allowed_tools: None,
            metered: false,
            rpm_limit: None,
        }
    }

    fn cfg_with_mcp(servers: Vec<McpServerConfig>) -> AppConfig {
        let mut cfg = working_config();
        cfg.mcp = McpConfig {
            servers,
            searxng: SearxngSettings::default(),
        };
        cfg
    }

    #[test]
    fn mcp_seed_is_legacy_servers_with_unified_credentials() {
        let seed = seeded_mcp_servers();
        // Phase 3: the legacy pair followed by the curated pair.
        assert_eq!(seed.len(), 4);
        let gh = &seed[0];
        assert_eq!(gh.id, "github");
        assert_eq!(gh.display_name, "GitHub");
        assert_eq!(gh.exposure, McpExposure::Local);
        assert_eq!(gh.transport, McpTransport::Stdio);
        assert_eq!(gh.command.as_deref(), Some("npx"));
        assert_eq!(gh.args, vec!["-y", "@modelcontextprotocol/server-github"]);
        assert_eq!(gh.env_keys, vec!["GITHUB_PERSONAL_ACCESS_TOKEN"]);
        assert_eq!(gh.cred_ref.as_deref(), Some(GITHUB_MCP_CRED_REF));
        assert_eq!(GITHUB_MCP_CRED_REF, "local-llm-service-manager/github-mcp");
        // The credential field's label (Pete, 2026-10-18): the
        // seed names the credential kind for both legacy servers.
        assert_eq!(
            gh.credential_hint.as_deref(),
            Some("GitHub personal access token — created under GitHub Settings, Developer settings, Personal access tokens")
        );
        let gd = &seed[1];
        assert_eq!(gd.id, "gdrive");
        assert_eq!(gd.cred_ref.as_deref(), Some(GDRIVE_MCP_CRED_REF));
        // Canonicalised env key, not the legacy divergent name.
        assert_eq!(gd.env_keys, vec!["GOOGLE_APPLICATION_CREDENTIALS"]);
        // File-shaped credential (§6.4): the hint says the stored
        // value is the credentials file's path, not its contents.
        assert_eq!(
            gd.credential_hint.as_deref(),
            Some("Path to the Google OAuth credentials JSON file — downloaded from Google Cloud Console, APIs & Services, Credentials; store the file path, not the file contents")
        );
        assert_eq!(gd.args, vec!["-y", "@modelcontextprotocol/server-gdrive"]);
        // default_config carries the seed.
        assert_eq!(AppConfig::default_config().mcp.servers, seed);
        validate(&cfg_with_mcp(seed)).expect("seed must validate");
    }

    #[test]
    fn mcp_curated_seed_entries_match_design() {
        let curated = curated_mcp_servers();
        assert_eq!(curated.len(), 2);
        let brave = &curated[0];
        assert_eq!(brave.id, "brave_search");
        assert_eq!(brave.display_name, "Brave Search");
        assert_eq!(brave.transport, McpTransport::Stdio);
        assert_eq!(brave.exposure, McpExposure::Gateway);
        assert_eq!(brave.command.as_deref(), Some("npx"));
        assert_eq!(
            brave.args,
            vec![
                "-y",
                "@brave/brave-search-mcp-server@2.1.4",
                "--transport",
                "stdio"
            ]
        );
        assert_eq!(brave.env_keys, vec!["BRAVE_API_KEY"]);
        assert_eq!(brave.cred_ref.as_deref(), Some(BRAVE_SEARCH_MCP_CRED_REF));
        assert_eq!(
            BRAVE_SEARCH_MCP_CRED_REF,
            "local-llm-service-manager/brave-search-mcp"
        );
        assert_eq!(
            brave.allowed_tools.as_deref(),
            Some(
                &[
                    "brave_web_search".to_string(),
                    "brave_llm_context".to_string()
                ][..]
            )
        );
        assert!(brave.metered);
        assert_eq!(brave.rpm_limit, Some(20));
        // The credential prompt's registration link (Pete,
        // 2026-10-18): Brave carries the Search API page;
        // web_fetch has no credential and no link.
        assert_eq!(brave.signup_url.as_deref(), Some(BRAVE_SEARCH_SIGNUP_URL));
        assert_eq!(BRAVE_SEARCH_SIGNUP_URL, "https://brave.com/search/api/");
        assert_eq!(
            brave.credential_hint.as_deref(),
            Some("Brave Search API key — the subscription key from the Brave Search API page")
        );
        let fetch = &curated[1];
        assert_eq!(fetch.id, "web_fetch");
        assert_eq!(fetch.display_name, "Web Fetch");
        assert_eq!(fetch.transport, McpTransport::Stdio);
        assert_eq!(fetch.exposure, McpExposure::Disabled);
        assert_eq!(fetch.command.as_deref(), Some("uvx"));
        assert_eq!(fetch.args, vec!["mcp-server-fetch@2026.10.10"]);
        assert!(fetch.env_keys.is_empty());
        assert_eq!(fetch.cred_ref, None);
        assert_eq!(
            fetch.allowed_tools.as_deref(),
            Some(&["fetch".to_string()][..])
        );
        assert!(!fetch.metered);
        assert_eq!(fetch.rpm_limit, None);
        assert_eq!(fetch.signup_url, None);
        assert_eq!(fetch.credential_hint, None);
        // A fresh default config carries all four entries and is
        // born with the curated-seeded marker set.
        let default = AppConfig::default_config();
        assert_eq!(default.mcp.servers.len(), 4);
        assert!(default.mcp_curated_seeded);
        assert_eq!(default.mcp.searxng, SearxngSettings::default());
        validate(&default).expect("default with curated seed must validate");
    }

    #[test]
    fn migrate_pre_phase3_config_gains_curated_exactly_once() {
        // A Phase 1/2-era file: an explicit mcp section holding only
        // the legacy pair, and NO mcp_curated_seeded key anywhere.
        let mut raw = serde_json::to_value(working_config()).unwrap();
        raw.as_object_mut().unwrap().remove("mcp_curated_seeded");
        raw["mcp"] = serde_json::json!({ "servers": [
            {
                "id": "github", "display_name": "GitHub", "transport": "stdio",
                "exposure": "local", "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-github"],
                "url": null, "env_keys": ["GITHUB_PERSONAL_ACCESS_TOKEN"],
                "cred_ref": "local-llm-service-manager/github-mcp",
                "allowed_tools": null, "metered": false, "rpm_limit": null
            },
            {
                "id": "gdrive", "display_name": "Google Drive", "transport": "stdio",
                "exposure": "local", "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-gdrive"],
                "url": null, "env_keys": ["GOOGLE_APPLICATION_CREDENTIALS"],
                "cred_ref": "local-llm-service-manager/gdrive-mcp",
                "allowed_tools": null, "metered": false, "rpm_limit": null
            }
        ]});
        let cfg = migrate(raw).expect("pre-Phase-3 config must migrate");
        let ids: Vec<&str> = cfg.mcp.servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["github", "gdrive", "brave_search", "web_fetch"]);
        assert!(cfg.mcp_curated_seeded);
        validate(&cfg).expect("migrated config validates");
        // Second load (of the now-Phase-3 shape): no duplication.
        let cfg2 = migrate(serde_json::to_value(&cfg).unwrap()).expect("second load");
        assert_eq!(cfg2.mcp.servers.len(), 4);
        assert!(cfg2.mcp_curated_seeded);
    }

    #[test]
    fn migrate_curated_entries_deleted_stay_deleted() {
        // A Phase 3 file whose operator deleted both curated entries:
        // the marker is present and set, so nothing is resurrected.
        let mut cfg = working_config();
        cfg.mcp
            .servers
            .retain(|s| s.id == "github" || s.id == "gdrive");
        assert_eq!(cfg.mcp.servers.len(), 2);
        assert!(cfg.mcp_curated_seeded);
        let back = migrate(serde_json::to_value(&cfg).unwrap()).expect("reload");
        let ids: Vec<&str> = back.mcp.servers.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["github", "gdrive"]);
        // An entirely empty registry with the marker set also stays empty.
        let mut cfg = working_config();
        cfg.mcp.servers.clear();
        let back = migrate(serde_json::to_value(&cfg).unwrap()).expect("reload empty");
        assert!(back.mcp.servers.is_empty());
    }

    #[test]
    fn searxng_settings_default_and_validation() {
        // Registry JSON written before the settings existed loads
        // with SearXNG disabled and an empty base.
        let mcp: McpConfig = serde_json::from_str(r#"{"servers":[]}"#).unwrap();
        assert_eq!(mcp.searxng, SearxngSettings::default());
        // Disabled: no constraint, even on a half-typed base.
        let mut cfg = cfg_with_mcp(vec![]);
        cfg.mcp.searxng = SearxngSettings {
            enabled: false,
            api_base: "not a url".to_string(),
        };
        validate(&cfg).expect("disabled searxng carries no constraint");
        // Enabled with an empty base is rejected.
        cfg.mcp.searxng = SearxngSettings {
            enabled: true,
            api_base: "  ".to_string(),
        };
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("searxng"), "{err}");
        // Enabled with a non-URL / wrong scheme is rejected.
        cfg.mcp.searxng.api_base = "ftp://example.com".to_string();
        assert!(validate(&cfg).is_err());
        cfg.mcp.searxng.api_base = "not-a-url".to_string();
        assert!(validate(&cfg).is_err());
        // Enabled with loopback http (the §6.3 self-host shape) and
        // with https both validate.
        cfg.mcp.searxng.api_base = "http://127.0.0.1:8080".to_string();
        validate(&cfg).expect("loopback http searxng validates");
        cfg.mcp.searxng.api_base = "https://search.example.com/".to_string();
        validate(&cfg).expect("https searxng validates");
    }

    #[test]
    fn mcp_signup_url_must_be_an_http_url_when_present() {
        // Absent and blank both count as "no link" and validate.
        let mut server = mcp_server("linky");
        validate(&cfg_with_mcp(vec![server.clone()])).expect("no signup_url validates");
        server.signup_url = Some("  ".to_string());
        validate(&cfg_with_mcp(vec![server.clone()])).expect("blank signup_url validates");
        // A real registration page validates (https, or http on
        // loopback — the parse_mcp_http_url rule the server URL
        // itself follows).
        server.signup_url = Some("https://brave.com/search/api/".to_string());
        validate(&cfg_with_mcp(vec![server.clone()])).expect("https signup_url validates");
        // Anything else is rejected at save, so a stored config
        // can never carry a link the prompt would open blindly.
        server.signup_url = Some("not a url".to_string());
        let err = validate(&cfg_with_mcp(vec![server.clone()]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("signup_url"), "{err}");
        server.signup_url = Some("ftp://example.com/keys".to_string());
        assert!(validate(&cfg_with_mcp(vec![server])).is_err());
    }

    #[test]
    fn mcp_partial_server_json_loads_with_defaults() {
        let s: McpServerConfig = serde_json::from_str(r#"{"id":"x"}"#).unwrap();
        assert_eq!(s.transport, McpTransport::Stdio);
        assert_eq!(s.exposure, McpExposure::Disabled);
        assert_eq!(s.command, None);
        assert!(s.args.is_empty());
        assert!(!s.metered);
        // Entries persisted before the credential label existed
        // carry no hint; the view resolves one from the seed.
        assert_eq!(s.credential_hint, None);
        let s: McpServerConfig =
            serde_json::from_str(r#"{"id":"x","credential_hint":"Example API key"}"#).unwrap();
        assert_eq!(s.credential_hint.as_deref(), Some("Example API key"));
    }

    #[test]
    fn mcp_id_rules() {
        for bad in ["", "GitHub", "1abc", "brave-search", "a b", "a.b", "_a"] {
            let err = validate(&cfg_with_mcp(vec![mcp_server(bad)]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("must match"), "id {bad:?}: {err}");
        }
        for good in ["a", "brave_search", "a1", "web_fetch2"] {
            validate(&cfg_with_mcp(vec![mcp_server(good)])).expect("valid id");
        }
        // Duplicate.
        let err = validate(&cfg_with_mcp(vec![mcp_server("dup"), mcp_server("dup")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate mcp server id"), "{err}");
    }

    #[test]
    fn mcp_stdio_command_allowlist_and_windows_basename() {
        // Missing command.
        let mut s = mcp_server("x");
        s.command = None;
        assert!(validate(&cfg_with_mcp(vec![s.clone()])).is_err());
        // Not allowlisted.
        s.command = Some("powershell".to_string());
        assert!(validate(&cfg_with_mcp(vec![s.clone()])).is_err());
        // Windows-style path + .cmd suffix normalises to npx.
        s.command = Some(r"C:\tools\node\npx.cmd".to_string());
        validate(&cfg_with_mcp(vec![s.clone()])).expect("windows npx.cmd basename");
        // Unix path + .exe, upper case.
        s.command = Some("/usr/local/bin/UVX.EXE".to_string());
        validate(&cfg_with_mcp(vec![s.clone()])).expect("uvx.exe basename");
        for ok in ["npx", "uvx", "python", "python3", "node", "docker", "deno"] {
            s.command = Some(ok.to_string());
            validate(&cfg_with_mcp(vec![s.clone()])).expect("allowlisted");
        }
    }

    #[test]
    fn mcp_banned_args_rejected() {
        for banned in ["--allow-private-ips", "--ignore-robots-txt"] {
            let mut s = mcp_server("x");
            s.args.push(banned.to_string());
            let err = validate(&cfg_with_mcp(vec![s])).unwrap_err().to_string();
            assert!(err.contains(banned), "{err}");
            assert!(err.contains("rejected"), "{err}");
        }
        // `=value` form is also the banned token.
        let mut s = mcp_server("x");
        s.args.push("--allow-private-ips=true".to_string());
        assert!(validate(&cfg_with_mcp(vec![s])).is_err());
    }

    #[test]
    fn mcp_http_url_rules() {
        let http_server = |url: Option<&str>| McpServerConfig {
            transport: McpTransport::Http,
            command: None,
            url: url.map(str::to_string),
            ..mcp_server("remote")
        };
        // Missing url.
        assert!(validate(&cfg_with_mcp(vec![http_server(None)])).is_err());
        // Unparseable / wrong scheme.
        for bad in [
            "not-a-url",
            "ftp://example.com",
            "https://",
            "https://exa mple.com",
        ] {
            assert!(
                validate(&cfg_with_mcp(vec![http_server(Some(bad))])).is_err(),
                "{bad}"
            );
        }
        // https off-loopback is fine.
        validate(&cfg_with_mcp(vec![http_server(Some(
            "https://example.com/mcp",
        ))]))
        .expect("https");
        // http off-loopback is not.
        assert!(validate(&cfg_with_mcp(vec![http_server(Some("http://example.com"))])).is_err());
        // http on loopback is allowed, in all three spellings (+ port).
        for ok in [
            "http://127.0.0.1:8080",
            "http://localhost/mcp",
            "http://[::1]:8080/mcp",
        ] {
            validate(&cfg_with_mcp(vec![http_server(Some(ok))])).expect("loopback http");
        }
    }

    #[test]
    fn mcp_gateway_with_env_keys_requires_cred_ref() {
        let mut s = mcp_server("gw");
        s.exposure = McpExposure::Gateway;
        s.env_keys = vec!["SOME_KEY".to_string()];
        s.cred_ref = None;
        let err = validate(&cfg_with_mcp(vec![s.clone()]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cred_ref"), "{err}");
        s.cred_ref = Some("local-llm-service-manager/gw".to_string());
        // Credential itself missing is NOT a save error — only the ref.
        validate(&cfg_with_mcp(vec![s.clone()])).expect("cred_ref named");
        // Local with env keys and no cred_ref is allowed by this rule.
        s.exposure = McpExposure::Local;
        s.cred_ref = None;
        validate(&cfg_with_mcp(vec![s])).expect("local needs no cred_ref by §3.2");
        // Gateway with no env keys needs no cred_ref.
        let mut s2 = mcp_server("gw2");
        s2.exposure = McpExposure::Gateway;
        validate(&cfg_with_mcp(vec![s2])).expect("gateway without env keys");
    }

    #[test]
    fn migrate_without_mcp_key_seeds_registry() {
        // Pre-feature config: a full current config JSON with the mcp
        // key removed.
        let mut raw = serde_json::to_value(working_config()).unwrap();
        raw.as_object_mut().unwrap().remove("mcp");
        let cfg = migrate(raw).expect("pre-feature config must migrate");
        assert_eq!(cfg.mcp.servers, seeded_mcp_servers());
        validate(&cfg).expect("seeded config validates");
    }

    #[test]
    fn migrate_with_explicit_mcp_preserves_it_verbatim() {
        // Explicit empty registry is preserved — no resurrection.
        let mut raw = serde_json::to_value(working_config()).unwrap();
        raw["mcp"] = serde_json::json!({ "servers": [] });
        let cfg = migrate(raw).expect("explicit empty mcp");
        assert!(cfg.mcp.servers.is_empty());
        // Explicit custom registry is preserved.
        let mut raw = serde_json::to_value(working_config()).unwrap();
        raw["mcp"] = serde_json::json!({ "servers": [{
            "id": "custom", "display_name": "Custom", "transport": "http",
            "exposure": "disabled", "command": null, "args": [],
            "url": "https://example.com/mcp", "env_keys": [],
            "cred_ref": null, "allowed_tools": null, "metered": true,
            "rpm_limit": 7
        }]});
        let cfg = migrate(raw).expect("explicit custom mcp");
        assert_eq!(cfg.mcp.servers.len(), 1);
        assert_eq!(cfg.mcp.servers[0].id, "custom");
        assert_eq!(cfg.mcp.servers[0].rpm_limit, Some(7));
        assert!(cfg.mcp.servers[0].metered);
    }
}

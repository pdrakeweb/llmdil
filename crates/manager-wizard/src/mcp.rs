//! Gateway MCP arming and MCP response parsing helpers.

use super::*;

/// ---------------------------------------------------------------------------
/// MCP gateway arming (MCP design §4.2) — THE armed-set rule.
///
/// A Gateway-exposed server is ARMED for a start when every secret
/// it needs can be resolved: it has no `env_keys`, OR its `cred_ref`
/// credential can be resolved by the caller-supplied predicate.
/// The predicate is over the Credential Manager TARGET NAME: the
/// start-gateway step resolves it by actually reading the credential
/// (`credential_read` via the ops layer), while the drift path
/// (src-tauri's audit) may resolve it by presence
/// (`credential_exists`) — an accepted semantic gap, documented at
/// [`gateway_config_is_current`]: the fingerprint hash below covers
/// the `cred_ref` NAME, never the value, so presence is enough to
/// determine set membership unless a credential exists but is
/// unreadable (corrupt blob), in which case drift reads as current
/// while a real start would omit the server and record a different
/// fingerprint on its next successful run.
///
/// This is the ONE definition of arming. YAML generation (through
/// [`config_with_armed_gateway_mcp_servers`]), the spawn
/// environment, the spawn fingerprint, and the drift check all
/// derive from it; no caller re-derives the rule.
pub fn armed_gateway_mcp_servers(
    config: &AppConfig,
    mut can_resolve: impl FnMut(&str) -> bool,
) -> Vec<&McpServerConfig> {
    config
        .mcp
        .servers
        .iter()
        .filter(|s| s.exposure == McpExposure::Gateway)
        .filter(|s| {
            s.env_keys.is_empty()
                || match s.cred_ref.as_deref().filter(|r| !r.is_empty()) {
                    Some(target) => can_resolve(target),
                    None => false,
                }
        })
        .collect()
}

/// A clone of `config` whose MCP registry holds ONLY the armed
/// Gateway servers (see [`armed_gateway_mcp_servers`]), in config
/// order. This is the config the gateway YAML is generated from for
/// a start: `manager_gateway::generate_config_yaml` stays a pure
/// function of the config it is given, Local/Disabled entries never
/// affect generation, and an unarmed Gateway server is omitted for
/// that start without touching the persisted registry.
pub fn config_with_armed_gateway_mcp_servers(
    config: &AppConfig,
    can_resolve: impl FnMut(&str) -> bool,
) -> AppConfig {
    let armed: Vec<McpServerConfig> = armed_gateway_mcp_servers(config, can_resolve)
        .into_iter()
        .cloned()
        .collect();
    let mut filtered = config.clone();
    filtered.mcp.servers = armed;
    filtered
}

/// Whether any server in an armed set is a stdio server — the
/// condition for setting `LITELLM_ENABLE_MCP_STDIO=true` in the
/// gateway spawn environment and for enforcing the §4.3 LiteLLM
/// version floor.
pub fn mcp_armed_set_has_stdio(servers: &[&McpServerConfig]) -> bool {
    servers.iter().any(|s| s.transport == McpTransport::Stdio)
}

/// SHA-256 hash of an armed registered set, covering the
/// generation-relevant fields of each server (id, transport,
/// command, args, url, env_keys, cred_ref, allowed_tools) in the
/// order given (callers pass config order — deterministic). Fields
/// are length-prefixed so concatenation cannot alias two different
/// sets to one byte stream. The hash of the EMPTY set is the empty
/// string, so pre-feature spawn-state records (which carry the
/// serde default `""`) still compare current for configurations
/// with no Gateway servers. Secret VALUES are never hashed here —
/// only the `cred_ref` target name.
pub fn mcp_armed_servers_hash(servers: &[&McpServerConfig]) -> String {
    use sha2::Digest as _;
    if servers.is_empty() {
        return String::new();
    }
    let mut hasher = sha2::Sha256::new();
    let field = |h: &mut sha2::Sha256, value: &str| {
        h.update((value.len() as u64).to_le_bytes());
        h.update(value.as_bytes());
    };
    for s in servers {
        field(&mut hasher, &s.id);
        field(
            &mut hasher,
            match s.transport {
                McpTransport::Stdio => "stdio",
                McpTransport::Http => "http",
            },
        );
        field(&mut hasher, s.command.as_deref().unwrap_or(""));
        // Distinguish "no command" from an empty command.
        field(&mut hasher, if s.command.is_some() { "1" } else { "0" });
        for arg in &s.args {
            field(&mut hasher, arg);
        }
        field(&mut hasher, "\u{1}args-end");
        field(&mut hasher, s.url.as_deref().unwrap_or(""));
        field(&mut hasher, if s.url.is_some() { "1" } else { "0" });
        for key in &s.env_keys {
            field(&mut hasher, key);
        }
        field(&mut hasher, "\u{1}env-end");
        field(&mut hasher, s.cred_ref.as_deref().unwrap_or(""));
        field(&mut hasher, if s.cred_ref.is_some() { "1" } else { "0" });
        match &s.allowed_tools {
            Some(tools) => {
                field(&mut hasher, "some");
                for t in tools {
                    field(&mut hasher, t);
                }
            }
            None => field(&mut hasher, "none"),
        }
        field(&mut hasher, "\u{1}server-end");
    }
    format!("{:x}", hasher.finalize())
}

/// Server ids in a LiteLLM `GET /v1/mcp/server` response body
/// (MCP design §8). Tolerant by design: LiteLLM's exact envelope
/// for this endpoint is a §14 residual unknown, so the parser
/// accepts a bare array of ids, an array of objects (id under
/// `server_id` / `id` / `name` / `mcp_server_id`), an object with a
/// `servers` (or `data`) array, and an object whose keys are the
/// ids. An unparseable body yields an empty list — callers treat
/// that as "no server is registered", which is the failing
/// direction for the load check.
pub fn parse_mcp_registered_server_ids(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    fn id_of(entry: &serde_json::Value) -> Option<String> {
        match entry {
            serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
            serde_json::Value::Object(map) => ["server_id", "id", "name", "mcp_server_id"]
                .iter()
                .find_map(|k| map.get(*k).and_then(|v| v.as_str()))
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            _ => None,
        }
    }
    match &value {
        serde_json::Value::Array(entries) => entries.iter().filter_map(id_of).collect(),
        serde_json::Value::Object(map) => {
            for key in ["servers", "data", "mcp_servers"] {
                if let Some(serde_json::Value::Array(entries)) = map.get(key) {
                    return entries.iter().filter_map(id_of).collect();
                }
            }
            // Object keyed by server id.
            map.keys().cloned().collect()
        }
        _ => Vec::new(),
    }
}

/// Tool names in a LiteLLM `GET /mcp-rest/tools/list` response
/// body (§8). Accepts an array of tool objects, or an object with a
/// `tools` (or `data`, or `result.tools`) array; a tool's name is
/// its `name` field (a bare string entry also counts).
pub fn parse_mcp_tool_names(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    fn names(entries: &[serde_json::Value]) -> Vec<String> {
        entries
            .iter()
            .filter_map(|e| match e {
                serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
                serde_json::Value::Object(map) => map
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                _ => None,
            })
            .collect()
    }
    match &value {
        serde_json::Value::Array(entries) => names(entries),
        serde_json::Value::Object(map) => {
            for key in ["tools", "data"] {
                if let Some(serde_json::Value::Array(entries)) = map.get(key) {
                    return names(entries);
                }
            }
            if let Some(result) = map.get("result") {
                if let Some(serde_json::Value::Array(entries)) = result.get("tools") {
                    return names(entries);
                }
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// The gateway's own error text in an MCP REST response body, when
/// it carries one: a top-level `error` (string, or object with a
/// `message`) — LiteLLM's error envelope for these endpoints.
/// `None` when the body carries no error (including unparseable
/// bodies; HTTP status is the caller's primary signal).
pub fn mcp_response_error(body: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let error = value.get("error")?;
    match error {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Object(map) => map
            .get("message")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| Some(error.to_string())),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Maximum length, in characters, of the response excerpt carried
/// in an MCP tool-call test result (MCP design §8, Phase 3). A real
/// Brave search response is far larger than anything a test result
/// should hold; the excerpt exists so the operator can see the call
/// produced plausible output, not to transport the output.
pub const MCP_TOOL_CALL_DETAIL_MAX_CHARS: usize = 200;

/// Build the JSON body for one `POST /mcp-rest/tools/call` request
/// (LiteLLM MCP REST API docs, docs.litellm.ai/docs/mcp_rest_api,
/// accessed 2026-10-17). The documented shape is
/// `{"server_id": ..., "name": ..., "arguments": {...}}` where
/// `name` may be the prefixed (`<server>-<tool>`) or the unprefixed
/// tool name; callers here pass the UNPREFIXED name together with
/// the server id, the combination the docs list as working
/// ("unprefixed name + server name"). `arguments` is always a JSON
/// object, never null (a null is a documented 500). This is the one
/// place the body is constructed, so a Phase 5 on-machine finding
/// about the shape changes exactly one function.
pub fn mcp_tool_call_body(
    server_id: &str,
    tool_name: &str,
    arguments: serde_json::Value,
) -> String {
    serde_json::json!({
        "server_id": server_id,
        "name": tool_name,
        "arguments": arguments,
    })
    .to_string()
}

/// The fixed, harmless real-call body for the curated Brave test
/// (MCP design §8): one `brave_web_search` call with a one-word
/// query. Metered — only ever sent from the explicit per-server
/// Test action, never from a start-time load check or a poll.
pub fn brave_search_test_call_body() -> String {
    mcp_tool_call_body(
        "brave_search",
        "brave_web_search",
        serde_json::json!({ "query": "llmdil" }),
    )
}

/// Summarize one `POST /mcp-rest/tools/call` response for a test
/// result payload: `(ok, detail)`. Ok requires HTTP 200 and no
/// error envelope ([`mcp_response_error`]). The detail is the
/// gateway's own error text when it sent one, else a truncated
/// excerpt of the body (capped at
/// [`MCP_TOOL_CALL_DETAIL_MAX_CHARS`] characters — the full search
/// response is never carried into a result payload).
pub fn summarize_mcp_tool_call_response(status: u16, body: &str) -> (bool, String) {
    if let Some(err) = mcp_response_error(body) {
        return (false, truncate_excerpt(&format!("tool call error: {err}")));
    }
    if status != 200 {
        return (
            false,
            truncate_excerpt(&format!("tool call failed: HTTP {status}")),
        );
    }
    (true, truncate_excerpt(body.trim()))
}

/// Truncate `text` to [`MCP_TOOL_CALL_DETAIL_MAX_CHARS`]
/// characters (on a char boundary), marking truncation with an
/// ellipsis suffix.
fn truncate_excerpt(text: &str) -> String {
    if text.chars().count() <= MCP_TOOL_CALL_DETAIL_MAX_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(MCP_TOOL_CALL_DETAIL_MAX_CHARS).collect();
    format!("{kept}…")
}

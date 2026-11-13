//! Client configuration writers: Continue config/env files and the VS Code LiteLLM endpoint + Copilot agents.

use super::*;

///
/// Shared by the [`VscodeExtensions`] wizard step and the Tauri
/// `vscode_write_config` command so both write byte-identical configs.
/// When `master_key` is given it is written as each model's `apiKey`:
/// the gateway requires it, and an editor client config is where such
/// a key conventionally lives (the user's own profile, on their own
/// machine).
/// The secret name Continue resolves for the gateway master key:
/// `config.yaml` references `${{ secrets.LITELLM_API_KEY }}` and
/// Continue's local profile resolves it from `~/.continue/.env`
/// (first match in its search order), so the key lives in the `.env`
/// file and never in the YAML.
pub const CONTINUE_ENV_KEY: &str = "LITELLM_API_KEY";

/// Render a string as a YAML scalar: plain when it is safe (letters,
/// digits, and the punctuation our generated values use), else a
/// JSON double-quoted scalar (valid YAML).
fn yaml_scalar(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || " _().-/:@".contains(c))
        && !s.starts_with(|c: char| "-?:,[]{}#&*!|>'\"%@`".contains(c));
    if safe {
        s.to_string()
    } else {
        serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
    }
}

/// The Continue `config.yaml` document for the local gateway
/// (Continue 1.x local profile format, verified against
/// continuedev/continue on 2026-10-15):
///
/// - Every gateway group becomes a model with `provider: openai`
///   (there is no dedicated LiteLLM provider; an unknown provider
///   name makes Continue silently skip the model), `model:` set to
///   the bare group name (sent to the gateway unchanged), and
///   `apiBase` WITH `/v1` (Continue appends `chat/completions`;
///   the opposite of the LiteLLM extension's no-`/v1` rule).
/// - `apiKey` references the secret in `~/.continue/.env` — no
///   literal key in the YAML.
/// - `roles: [chat, edit, apply]` puts the models in the chat
///   picker and in Edit/Apply; `capabilities: [tool_use]` is
///   required for Agent mode because Continue's tool-support
///   autodetection only recognizes known cloud model names.
/// - Groups keep config order: with no stored selection, the chat
///   picker starts at the first chat model (the planner).
pub fn continue_config_yaml(config: &AppConfig) -> String {
    let mut out = String::new();
    out.push_str("name: Local LLM Gateway\nversion: 1.0.0\nschema: v1\nmodels:\n");
    for g in &config.gateway.groups {
        out.push_str(&format!(
            "  - name: {}\n    provider: openai\n    model: {}\n    apiBase: http://127.0.0.1:{}/v1\n    apiKey: ${{{{ secrets.LITELLM_API_KEY }}}}\n    roles:\n      - chat\n      - edit\n      - apply\n    capabilities:\n      - tool_use\n    defaultCompletionOptions:\n      contextLength: 32768\n",
            yaml_scalar(&format!("{} (local)", g.name)),
            yaml_scalar(&g.name),
            config.gateway.port
        ));
    }
    out
}

/// The gateway models of [`continue_config_yaml`] as YAML values,
/// for merging into an existing document.
fn continue_yaml_models(config: &AppConfig) -> Vec<serde_yaml::Value> {
    let doc: serde_yaml::Value =
        serde_yaml::from_str(&continue_config_yaml(config)).unwrap_or(serde_yaml::Value::Null);
    doc.get("models")
        .and_then(|m| m.as_sequence().cloned())
        .unwrap_or_default()
}

/// Merge the gateway models into an existing Continue `config.yaml`:
/// the wizard's own entries (identified by `model:` + `apiBase`, per
/// the config research) are replaced with the generated ones; every
/// other model and every other top-level key is preserved as a
/// value. The document is re-serialized, so comments and formatting
/// in the existing file are normalized — the caller logs that.
/// Errors when the existing text is not a YAML mapping, so the
/// caller can fall back to a sidecar instead of clobbering it.
pub fn merge_continue_config_yaml(existing: &str, config: &AppConfig) -> Result<String, String> {
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(existing).map_err(|e| format!("existing config.yaml: {e}"))?;
    if doc.is_null() {
        doc = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let map = doc
        .as_mapping_mut()
        .ok_or_else(|| "existing config.yaml is not a mapping".to_string())?;
    let api_base = format!("http://127.0.0.1:{}/v1", config.gateway.port);
    let our_names: Vec<String> = config
        .gateway
        .groups
        .iter()
        .map(|g| g.name.clone())
        .collect();
    let mut models: Vec<serde_yaml::Value> = map
        .get("models")
        .and_then(|m| m.as_sequence().cloned())
        .unwrap_or_default();
    models.retain(|m| {
        let model = m.get("model").and_then(|v| v.as_str());
        let base = m.get("apiBase").and_then(|v| v.as_str());
        !(base == Some(api_base.as_str())
            && model
                .map(|n| our_names.iter().any(|o| o == n))
                .unwrap_or(false))
    });
    models.extend(continue_yaml_models(config));
    map.insert(
        serde_yaml::Value::String("models".to_string()),
        serde_yaml::Value::Sequence(models),
    );
    for (k, v) in [
        ("name", "Local LLM Gateway"),
        ("version", "1.0.0"),
        ("schema", "v1"),
    ] {
        map.entry(serde_yaml::Value::String(k.to_string()))
            .or_insert_with(|| serde_yaml::Value::String(v.to_string()));
    }
    serde_yaml::to_string(&doc).map_err(|e| format!("serialize merged config.yaml: {e}"))
}

/// Set `KEY=value` in dotenv text: an existing `KEY=` line (with or
/// without an `export` prefix) is replaced; otherwise the line is
/// appended. All other lines — comments and other variables —
/// survive unchanged.
pub fn merge_env_contents(existing: &str, key: &str, value: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut written = false;
    for line in existing.lines() {
        let trimmed = line.trim_start();
        let body = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        if body.starts_with(&format!("{key}=")) {
            if !written {
                out.push(format!("{key}={value}"));
                written = true;
            }
            continue;
        }
        out.push(line.to_string());
    }
    if !written {
        out.push(format!("{key}={value}"));
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// True when a `~/.continue/config.json` is one the wizard itself
/// wrote: a JSON object whose every model entry points at this
/// gateway (provider `openai`, our `apiBase`). A config.json with
/// any other content is the user's own and is left alone.
pub fn legacy_continue_json_is_ours(text: &str, config: &AppConfig) -> bool {
    let api_base = format!("http://127.0.0.1:{}/v1", config.gateway.port);
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    let Some(models) = v.get("models").and_then(|m| m.as_array()) else {
        return false;
    };
    !models.is_empty()
        && models.iter().all(|m| {
            m.get("provider").and_then(|p| p.as_str()) == Some("openai")
                && m.get("apiBase").and_then(|b| b.as_str()) == Some(api_base.as_str())
        })
}

/// Outcome of [`write_continue_files`].
pub struct ContinueWrite {
    pub log: Vec<String>,
    /// True when the canonical files under `~/.continue` were
    /// written (false for staged-only and sidecar fallback).
    pub applied: bool,
}

/// Write Continue's configuration for the local gateway: the
/// `config.yaml` (models) + `.env` (master key) pair. Always stages
/// both files under `<data_dir>/vscode/`; with a home directory,
/// also applies them under `~/.continue`:
///
/// - `config.yaml`: merged into an existing file (see
///   [`merge_continue_config_yaml`]); when the existing file cannot
///   be parsed as YAML, ours is written to a sidecar
///   (`config.yaml.llm-manager`) instead of clobbering it.
/// - `.env`: the `LITELLM_API_KEY` line is merged by key.
/// - A wizard-written legacy `config.json` is moved aside to
///   `config.json.llm-manager.bak`: once `config.yaml` exists the
///   JSON is inert, but if the YAML were later deleted the JSON —
///   with its stale literal key — would silently become live again.
pub async fn write_continue_files(
    ops: &dyn SystemOps,
    config: &AppConfig,
    home: Option<&Path>,
    data_dir: &Path,
) -> Result<ContinueWrite, WizardError> {
    let key = crate::ensure_gateway_master_key(ops).await?;
    let yaml = continue_config_yaml(config);
    let env_text = format!("{CONTINUE_ENV_KEY}={key}\n");
    let mut log = Vec::new();
    let staged_dir = data_dir.join("vscode");
    ops.write_file(&staged_dir.join("continue-config.yaml"), yaml.as_bytes())
        .await?;
    ops.write_file(&staged_dir.join("continue.env"), env_text.as_bytes())
        .await?;
    log.push(format!(
        "staged Continue config at {}",
        staged_dir.join("continue-config.yaml").display()
    ));
    let Some(home) = home else {
        log.push("home directory unknown; Continue config staged only".to_string());
        return Ok(ContinueWrite {
            log,
            applied: false,
        });
    };
    let dir = home.join(".continue");
    // config.yaml: write fresh, or merge into the existing file.
    let yaml_path = dir.join("config.yaml");
    let mut applied = true;
    if ops.file_exists(&yaml_path).await {
        let existing = ops.read_file(&yaml_path).await.unwrap_or_default();
        let existing = String::from_utf8_lossy(&existing).into_owned();
        match merge_continue_config_yaml(&existing, config) {
            Ok(merged) => {
                ops.write_file(&yaml_path, merged.as_bytes()).await?;
                log.push(format!(
                    "merged gateway models into existing {} (comments and formatting normalized)",
                    yaml_path.display()
                ));
            }
            Err(e) => {
                let sidecar = dir.join("config.yaml.llm-manager");
                ops.write_file(&sidecar, yaml.as_bytes()).await?;
                log.push(format!(
                    "existing {} could not be merged ({e}); generated config at {}",
                    yaml_path.display(),
                    sidecar.display()
                ));
                applied = false;
            }
        }
    } else {
        ops.write_file(&yaml_path, yaml.as_bytes()).await?;
        log.push(format!("wrote {}", yaml_path.display()));
    }
    // .env: merge the key line into whatever is already there.
    let env_path = dir.join(".env");
    let existing_env = match ops.read_file(&env_path).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    };
    let merged_env = merge_env_contents(&existing_env, CONTINUE_ENV_KEY, &key);
    ops.write_file(&env_path, merged_env.as_bytes()).await?;
    log.push(format!(
        "Continue secret {CONTINUE_ENV_KEY} set in {}",
        env_path.display()
    ));
    // Retire a wizard-written legacy config.json (see fn docs).
    let legacy = dir.join("config.json");
    if ops.file_exists(&legacy).await {
        let bytes = ops.read_file(&legacy).await.unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        if legacy_continue_json_is_ours(&text, config) {
            let bak = dir.join("config.json.llm-manager.bak");
            ops.write_file(&bak, &bytes).await?;
            ops.remove_file(&legacy).await?;
            log.push(format!(
                "retired legacy Continue config.json to {} (config.yaml supersedes it)",
                bak.display()
            ));
        }
    }
    Ok(ContinueWrite { log, applied })
}

/// The label of the manager's own entry in the LiteLLM extension's
/// `litellm-vscode-chat.servers` setting. Labels are the extension's
/// server identities: the wizard replaces its entry by label (or by
/// base URL), never appends a duplicate.
pub const LITELLM_SERVER_LABEL: &str = "Local";

/// The manager's LiteLLM server entry for the VS Code extension:
/// base URL WITHOUT `/v1` (the extension appends it; with it, model
/// discovery doubles the path and 404s) and the gateway master key as
/// the API key (the extension supports an inline `auth.apiKey` and it
/// takes precedence over its secret storage).
pub fn litellm_server_entry(gateway_port: u16, master_key: &str) -> serde_json::Value {
    serde_json::json!({
        "label": LITELLM_SERVER_LABEL,
        "baseUrl": format!("http://127.0.0.1:{gateway_port}"),
        "auth": { "apiKey": master_key },
    })
}

fn cst_input_value(v: &serde_json::Value) -> jsonc_parser::cst::CstInputValue {
    use jsonc_parser::cst::CstInputValue;
    match v {
        serde_json::Value::Null => CstInputValue::Null,
        serde_json::Value::Bool(b) => CstInputValue::Bool(*b),
        serde_json::Value::Number(n) => CstInputValue::Number(n.to_string()),
        serde_json::Value::String(s) => CstInputValue::String(s.clone()),
        serde_json::Value::Array(items) => {
            CstInputValue::Array(items.iter().map(cst_input_value).collect())
        }
        serde_json::Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), cst_input_value(v)))
                .collect(),
        ),
    }
}

/// Merge the manager's LiteLLM server entry into VS Code `settings.json`
/// text. The file is JSONC (comments, trailing commas): the edit goes
/// through a concrete syntax tree so the user's comments, formatting,
/// and unrelated settings survive untouched. Only the
/// `litellm-vscode-chat.servers` array is rewritten — entries the
/// wizard does not own are carried over as values; its own entry
/// (matched by label or base URL) is replaced, never duplicated.
pub fn merge_litellm_server_setting(
    settings_text: &str,
    entry: &serde_json::Value,
) -> Result<String, String> {
    use jsonc_parser::cst::CstRootNode;
    let parse_options = jsonc_parser::ParseOptions::default();
    let current = jsonc_parser::parse_to_serde_value(settings_text, &parse_options)
        .map_err(|e| format!("could not parse VS Code settings: {e}"))?;
    let mut servers: Vec<serde_json::Value> = current
        .as_ref()
        .and_then(|v| v.get("litellm-vscode-chat.servers"))
        .and_then(|s| s.as_array().cloned())
        .unwrap_or_default();
    let base = entry
        .get("baseUrl")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    servers.retain(|s| {
        s.get("label").and_then(|v| v.as_str()) != Some(LITELLM_SERVER_LABEL)
            && s.get("baseUrl").and_then(|v| v.as_str()) != Some(base)
    });
    servers.push(entry.clone());
    let root = CstRootNode::parse(settings_text, &parse_options)
        .map_err(|e| format!("could not parse VS Code settings: {e}"))?;
    let obj = root.object_value_or_set();
    let value = cst_input_value(&serde_json::Value::Array(servers));
    match obj.get("litellm-vscode-chat.servers") {
        Some(prop) => prop.set_value(value),
        None => {
            obj.append("litellm-vscode-chat.servers", value);
        }
    }
    Ok(root.to_string())
}

/// The Copilot custom-agent file for one gateway group, written to
/// `~/.copilot/agents/litellm-<group>.md`. The planner text is Pete's
/// own (2026-10-15, verbatim); the other roles follow its structure.
/// Group names are the gateway's model names, exposed by the LiteLLM
/// extension under the `litellm` vendor id.
pub fn copilot_agent_markdown(group: &str) -> String {
    match group {
        "planner" => r##"---
name: LiteLLM Planner
description: "Use for implementation planning, architecture and design planning, technical breakdowns, sequencing work, and evaluating trade-offs. Produces actionable plans without implementing changes."
model: "litellm/planner"
tools: [read, search]
user-invocable: true
---
You are a software planning agent. Use the `litellm/planner` model to help turn goals into clear, evidence-based plans. Support implementation planning, architecture and design planning, and general technical planning.

## Boundaries
- Plan and analyze; do not edit files, run commands, or implement changes.
- Ground repository-specific recommendations in the files and symbols you inspect. Distinguish verified facts from assumptions.
- Keep the plan scoped to the request and identify material decisions or missing information instead of silently inventing requirements.

## Approach
1. Restate the goal and identify constraints, success criteria, and important unknowns.
2. Inspect relevant project files and existing patterns before proposing changes.
3. Break the work into ordered, concrete steps; identify dependencies and affected areas.
4. Include appropriate tests or validation, risks, and alternatives when they matter.
5. Keep the plan proportionate to the task; avoid speculative or unrelated work.

## Output
Provide a concise plan with:
- Goal and assumptions
- Ordered steps, including dependencies where relevant
- Files or components likely to be affected, when known
- Validation and risks or open decisions, when relevant
"##
        .to_string(),
        "coder" => r##"---
name: LiteLLM Coder
description: "Use for implementing features, fixing defects, and refactoring code. Writes and edits code, runs builds and tests, and verifies the result."
model: "litellm/coder"
tools: [read, search, edit, execute]
user-invocable: true
---
You are a software implementation agent. Use the `litellm/coder` model to turn a defined task or plan into working, verified code.

## Boundaries
- Implement the requested change; do not expand scope into unrelated refactors or features.
- Follow the repository's existing patterns, libraries, and style. Inspect before writing.
- Do not commit, push, or publish unless the user asks.

## Approach
1. Restate the task and identify the files and components involved.
2. Inspect the relevant code and existing patterns before editing.
3. Make the smallest change that fully implements the task.
4. Run the project's builds, tests, or checks that cover the change; fix failures you introduced.
5. Report what changed and what was verified, including anything left unverified.

## Output
Provide a concise summary with:
- What was implemented
- Files changed
- Validation run and its result
- Open issues or follow-ups, when relevant
"##
        .to_string(),
        "coder-fast" => r##"---
name: LiteLLM Coder Fast
description: "Use for small, well-scoped edits: quick fixes, boilerplate, renames, and minor adjustments where turnaround matters more than depth."
model: "litellm/coder-fast"
tools: [read, search, edit, execute]
user-invocable: true
---
You are a fast software implementation agent. Use the `litellm/coder-fast` model for small, clearly scoped changes.

## Boundaries
- Keep the change minimal and local; if the task turns out to be large or ambiguous, stop and say so instead of guessing.
- Follow existing patterns in the files you touch; do not refactor beyond the requested edit.

## Approach
1. Locate the exact code involved.
2. Make the edit.
3. Run the narrowest available check (build, test, or lint) that covers it.
4. Report the change and the check result in a few lines.

## Output
- What changed and where
- Check run and result
"##
        .to_string(),
        "tool-runner" => r##"---
name: LiteLLM Tool Runner
description: "Use for executing defined tasks and command-driven work: running scripts, builds, data gathering, and multi-step operational procedures, reporting observed results."
model: "litellm/tool-runner"
tools: [read, search, execute]
user-invocable: true
---
You are a task execution agent. Use the `litellm/tool-runner` model to carry out defined procedures and command-driven work accurately.

## Boundaries
- Execute the task as specified; do not redesign it. Surface ambiguity instead of inventing steps.
- Do not edit source files unless the task explicitly calls for it.
- Report observed output and failures exactly; never report a step as done that did not run.

## Approach
1. Restate the task as an ordered list of steps before running anything.
2. Execute each step, checking exit status and output before moving on.
3. On failure, stop, preserve the error output, and report which step failed and why.
4. Summarize the final state against the task's success criteria.

## Output
- Steps run and their results
- Final state vs. the success criteria
- Failures or deviations, when any
"##
        .to_string(),
        other => format!(
            r##"---
name: LiteLLM {other}
description: "Use for software implementation work with the {other} model: implementing features, fixing defects, and verifying the result."
model: "litellm/{other}"
tools: [read, search, edit, execute]
user-invocable: true
---
You are a software implementation agent. Use the `litellm/{other}` model to turn a defined task into working, verified code.

## Boundaries
- Implement the requested change; do not expand scope into unrelated work.
- Follow the repository's existing patterns, libraries, and style. Inspect before writing.

## Approach
1. Restate the task and identify the files and components involved.
2. Inspect the relevant code before editing.
3. Make the smallest change that fully implements the task.
4. Run the project's builds, tests, or checks that cover the change.
5. Report what changed and what was verified.

## Output
- What was implemented
- Files changed
- Validation run and its result
"##
        ),
    }
}

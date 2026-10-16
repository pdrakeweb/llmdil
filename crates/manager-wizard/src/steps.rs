//! The ten concrete wizard steps (spec §4.1).
//!
//! Steps branch on [`RunMode`]: in `Install` they perform the install; in
//! `Audit` they re-verify without reinstalling. All system effects go through
//! [`SystemOps`](crate::SystemOps), so every step is hermetic under
//! [`FakeSystemOps`](crate::fake::FakeSystemOps).

use crate::{
    checksums_match, is_pinned_hash, PlannedCommand, RunMode, StepOutcome, SystemOps,
    WizardContext, WizardError, WizardStep,
};
use async_trait::async_trait;
use manager_config::{AppConfig, BackendConfig, ModelConfig, SplitMode};
use std::path::{Path, PathBuf};
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

/// Stable step ids.
pub const STEP_DETECT_HARDWARE: &str = "detect-hardware";
pub const STEP_INSTALL_LLAMACPP: &str = "install-llamacpp";
pub const STEP_CONFIGURE_TENSOR_SPLIT: &str = "configure-tensor-split";
pub const STEP_DOWNLOAD_MODELS: &str = "download-models";
pub const STEP_INSTALL_POSTGRES: &str = "install-postgres";
pub const STEP_START_GATEWAY: &str = "start-gateway";
pub const STEP_VSCODE_EXTENSIONS: &str = "vscode-extensions";
pub const STEP_MCP_SERVERS: &str = "mcp-servers";
pub const STEP_WINML_REGISTER: &str = "winml-register";
pub const STEP_INSTALL_MXC: &str = "install-mxc";
pub const STEP_MXC_POLICIES: &str = "mxc-policies";
pub const STEP_SMOKE_TEST: &str = "smoke-test";

/// Resolve a possibly-relative path against the data dir.
fn resolve_dest(data_dir: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        data_dir.join(p)
    }
}

/// Serialize an already-JSON `serde_json::Value` without panicking.
///
/// Serialization of a `Value` is infallible in practice (no user `Serialize`
/// impls are involved); the debug fallback exists only to honor the
/// no-panic rule — it should never trigger.
fn json_pretty(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| format!("{v:#?}"))
}

/// Build the eleven steps in spec order.
pub fn default_steps() -> Vec<Box<dyn WizardStep>> {
    vec![
        Box::new(DetectHardware),
        Box::new(InstallLlamaCpp::with_pin(LlamaCppPin::default_pin())),
        Box::new(ConfigureTensorSplit),
        Box::new(DownloadModels),
        Box::new(InstallPostgres::with_pin(PostgresPin::default_pin())),
        Box::new(StartGateway::default()),
        Box::new(VscodeExtensions::default()),
        Box::new(McpServers),
        Box::new(WinMlRegister),
        Box::new(InstallMxc::with_pin(MxcSdkPin::default_pin())),
        Box::new(MxcPolicies),
        Box::new(SmokeTest::default()),
    ]
}

// ---------------------------------------------------------------------------
// 1. Detect hardware and drivers
// ---------------------------------------------------------------------------

/// Step 1: GPU inventory + driver check (§3.3 verification matrix).
pub struct DetectHardware;

#[async_trait]
impl WizardStep for DetectHardware {
    fn id(&self) -> &str {
        STEP_DETECT_HARDWARE
    }
    fn name(&self) -> &str {
        "Detect hardware and drivers"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, _ctx: &WizardContext) -> Vec<PlannedCommand> {
        // Exactly what RealSystemOps::gpu_inventory runs, in order: the
        // per-GPU query, then the plain nvidia-smi header for the driver
        // version. The program is manager-telemetry's System32-first
        // resolution (same as the real probe), not a bare PATH lookup.
        let smi = manager_telemetry::default_smi_path()
            .to_string_lossy()
            .into_owned();
        vec![
            PlannedCommand::new(vec![
                smi.clone(),
                "--query-gpu=index,name,uuid,memory.total,compute_cap".to_string(),
                "--format=csv,noheader".to_string(),
            ]),
            PlannedCommand::new(vec![smi]),
        ]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("install a current NVIDIA driver (R550+, i.e. 551.61+) and re-run the hardware check — download: https://www.nvidia.com/Download/index.aspx")
    }
    fn help_url(&self) -> Option<&str> {
        Some(crate::NVIDIA_DRIVER_DOWNLOAD_URL)
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let gpus = match ctx.ops.gpu_inventory().await {
            Ok(g) => g,
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!("GPU probe failed: {e}"),
                    vec![format!("{e:?}")],
                ))
            }
        };
        if gpus.is_empty() {
            return Ok(StepOutcome::failed(
                "no NVIDIA GPUs detected",
                vec!["GPU inventory probe returned zero devices".to_string()],
            ));
        }
        let mut log = Vec::new();
        for g in &gpus {
            log.push(format!(
                "gpu{}: {} ({}) — {} VRAM, sm_{}{}, driver {}",
                g.index,
                g.name,
                g.uuid,
                manager_present::mib(g.total_vram_mib),
                g.compute_capability.0,
                g.compute_capability.1,
                g.driver_version
            ));
        }
        let driver = &gpus[0].driver_version;
        let major: u32 = driver.split('.').next().unwrap_or("0").parse().unwrap_or(0);
        if major < crate::MIN_DRIVER_MAJOR {
            return Ok(StepOutcome::failed(
                format!(
                    "driver {driver} too old: need R{}+ for the pinned CUDA 12.x runtime (spec §3.4)",
                    crate::MIN_DRIVER_MAJOR
                ),
                log,
            ));
        }
        // PSEC/ingress probe result is recorded by the MXC step; here we just
        // note the driver/CUDA line for the report.
        Ok(StepOutcome::done(
            format!(
                "{} GPU(s) detected, driver {driver} (CUDA 12.x runtime compatible)",
                gpus.len()
            ),
            log,
        ))
    }
}

// ---------------------------------------------------------------------------
// 2. Install llama.cpp CUDA build
// ---------------------------------------------------------------------------

/// One archive of a pinned llama.cpp release: file name + expected SHA-256.
#[derive(Debug, Clone)]
pub struct LlamaCppAsset {
    pub asset: String,
    pub sha256: String,
}

/// Pinned llama.cpp build (spec §3.4).
///
/// The pin is compiled into the build: [`LlamaCppPin::default_pin`] carries
/// the tag and SHA-256 of both upstream archives (the binaries archive and
/// the companion CUDA-runtime archive — the binaries ship without the
/// cudart/cublas DLLs, so both are required). A release covers exactly one
/// tag; `updates.components.llamacpp.pinned_version` acts as a guard, not a
/// source: naming any other version fails closed in the step, because this
/// build carries no verified pin for it. [`LlamaCppPin::validate_for_gpus`]
/// enforces the Pascal rule — cuda-12.4 assets carry the sm_60 targets,
/// cuda-13.x dropped them.
#[derive(Debug, Clone)]
pub struct LlamaCppPin {
    pub tag: String,
    pub asset: String,
    pub sha256: String,
    /// Companion CUDA-runtime archive (cudart/cublas DLLs), same release.
    pub runtime: LlamaCppAsset,
}

impl LlamaCppPin {
    pub const BASE_URL: &'static str = "https://github.com/ggml-org/llama.cpp/releases/download";

    /// The verified pin shipped with this build: ggml-org llama.cpp
    /// **b11658**, cuda-12.4 x64. Both SHA-256 hashes were computed from
    /// the official release downloads on 2026-10-15 (not copied from a
    /// listing), and both archives were content-checked: the binaries
    /// archive carries `llama-server.exe` and `ggml-cuda.dll` at its root
    /// but no CUDA runtime DLLs; the runtime archive carries exactly
    /// `cublas64_12.dll`, `cublasLt64_12.dll`, and `cudart64_12.dll`.
    /// Both extract flat into the install dir.
    pub fn default_pin() -> Self {
        Self {
            tag: "b11658".to_string(),
            asset: "llama-b11658-bin-win-cuda-12.4-x64.zip".to_string(),
            sha256: "4268543cfffee11e62dbe1fb4ee907da508c2a70de7b845ed7b338bf8cabd76d".to_string(),
            runtime: LlamaCppAsset {
                asset: "cudart-llama-bin-win-cuda-12.4-x64.zip".to_string(),
                sha256: "8c79a9b226de4b3cacfd1f83d24f962d0773be79f1e7b75c6af4ded7e32ae1d6"
                    .to_string(),
            },
        }
    }

    pub fn download_url(&self) -> String {
        self.asset_url(&self.asset)
    }

    pub fn runtime_download_url(&self) -> String {
        self.asset_url(&self.runtime.asset)
    }

    fn asset_url(&self, asset: &str) -> String {
        format!("{}/{}/{}", Self::BASE_URL, self.tag, asset)
    }

    /// Both archives of the release, binaries first, in download order.
    fn archives(&self) -> [LlamaCppAsset; 2] {
        [
            LlamaCppAsset {
                asset: self.asset.clone(),
                sha256: self.sha256.clone(),
            },
            self.runtime.clone(),
        ]
    }

    /// Refuse cuda-13.x assets on Pascal-bearing machines (spec §3.4).
    /// Both archive names are checked: a cuda-13 runtime archive is just
    /// as unusable on Pascal as a cuda-13 binaries archive.
    pub fn validate_for_gpus(&self, gpus: &[crate::GpuDescriptor]) -> Result<(), WizardError> {
        let has_pascal = gpus.iter().any(|g| g.is_pascal_or_older());
        if !has_pascal {
            return Ok(());
        }
        for archive in self.archives() {
            let asset = archive.asset.to_lowercase();
            if asset.contains("cuda-13") || asset.contains("cuda13") {
                return Err(WizardError::UnsupportedBuild {
                    asset: archive.asset.clone(),
                    reason: "cuda-13.x builds dropped Pascal (sm_60) support; \
                             pin a cuda-12.4 asset (spec §3.4)"
                        .to_string(),
                });
            }
        }
        Ok(())
    }
}

/// Step 2: download the pinned llama.cpp CUDA build (binaries archive +
/// CUDA-runtime archive), verify both SHA-256s, extract both flat into
/// the app dir. Audit mode re-verifies the installed build.
pub struct InstallLlamaCpp {
    pub pin: Option<LlamaCppPin>,
}

impl InstallLlamaCpp {
    pub fn unconfigured() -> Self {
        Self { pin: None }
    }
    pub fn with_pin(pin: LlamaCppPin) -> Self {
        Self { pin: Some(pin) }
    }

    fn install_dir(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("llamacpp")
    }

    /// The MSVC-built llama.cpp CUDA binaries need the VC++ runtime.
    /// Probe: vcruntime140.dll loadable from System32. Windows-only;
    /// other platforms always pass.
    fn vcredist_present(system32: &std::path::Path) -> bool {
        if !cfg!(target_os = "windows") {
            return true;
        }
        system32.join("vcruntime140.dll").exists()
    }

    fn system32_dir() -> PathBuf {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        PathBuf::from(root).join("System32")
    }

    fn download_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        let Some(pin) = self.pin.as_ref() else {
            return Vec::new();
        };
        let dir = self.install_dir(ctx);
        pin.archives()
            .iter()
            .map(|archive| {
                PlannedCommand::new(vec![
                    "download".to_string(),
                    pin.asset_url(&archive.asset),
                    dir.join(&archive.asset).display().to_string(),
                ])
            })
            .collect()
    }
}

#[async_trait]
impl WizardStep for InstallLlamaCpp {
    fn id(&self) -> &str {
        STEP_INSTALL_LLAMACPP
    }
    fn name(&self) -> &str {
        "Install llama.cpp CUDA build"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_DETECT_HARDWARE.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        self.download_commands(ctx)
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("re-run install-llamacpp to re-download and re-verify the build; if llama-server fails to start, install the VC++ redistributable first")
    }
    fn help_url(&self) -> Option<&str> {
        Some(crate::VC_REDIST_DOWNLOAD_URL)
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let pin = match &self.pin {
            Some(p) => p.clone(),
            None => {
                return Ok(StepOutcome::failed(
                    "no llama.cpp build pinned",
                    vec![
                        "this build ships without a built-in llama.cpp pin; that is a packaging error in the app, not a configuration problem".to_string(),
                        "failing closed: will not download an unpinned binary".to_string(),
                    ],
                ))
            }
        };
        // Config guard: updates.components.llamacpp.pinned_version may name
        // the version the operator expects, but this build carries a
        // verified pin for exactly one tag. A different version is refused
        // (fail closed), never silently ignored or downloaded unverified.
        if let Some(wanted) = ctx
            .config
            .updates
            .components
            .get("llamacpp")
            .and_then(|c| c.pinned_version.as_deref())
        {
            if wanted != pin.tag {
                return Ok(StepOutcome::failed(
                    format!(
                        "updates.components.llamacpp pins llama.cpp {wanted}, but this build carries a verified pin only for {}",
                        pin.tag
                    ),
                    vec![
                        format!(
                            "operator-pinned version: {wanted}; verified built-in pin: {}",
                            pin.tag
                        ),
                        "failing closed: will not download a build without a verified SHA-256 pin"
                            .to_string(),
                    ],
                ));
            }
        }
        // VC++ runtime check before any download: the MSVC-built CUDA
        // binaries fail to start without it, with a cryptic DLL error.
        if ctx.mode == RunMode::Install && !Self::vcredist_present(&Self::system32_dir()) {
            return Ok(StepOutcome::failed(
                "VC++ redistributable not detected (vcruntime140.dll missing from System32)",
                vec![
                    "the llama.cpp CUDA build needs the Visual C++ runtime".to_string(),
                    format!(
                        "download and install it, then re-run this step: {}",
                        crate::VC_REDIST_DOWNLOAD_URL
                    ),
                ],
            ));
        }
        // Pascal rule, checked before any download.
        let gpus = ctx.ops.gpu_inventory().await.unwrap_or_default();
        if let Err(e) = pin.validate_for_gpus(&gpus) {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }

        let dir = self.install_dir(ctx);
        let marker = dir.join(".installed.json");
        let archives = pin.archives();
        let mut log = vec![
            format!("pinned build: {} ({})", pin.asset, pin.tag),
            format!("pinned runtime: {} ({})", pin.runtime.asset, pin.tag),
        ];

        match ctx.mode {
            RunMode::Install => {
                // Both archives are required: the binaries archive has no
                // CUDA runtime DLLs, the runtime archive has no binaries.
                // Each is downloaded, SHA-256-verified, and extracted flat
                // into the install dir; an archive already on disk with a
                // matching checksum is kept (per-archive skip).
                for archive_pin in &archives {
                    let archive = dir.join(&archive_pin.asset);
                    let archive_ok = ctx.ops.file_exists(&archive).await
                        && matches!(ctx.ops.sha256_file(&archive).await, Ok(h) if checksums_match(&h, &archive_pin.sha256));
                    if !archive_ok {
                        let url = pin.asset_url(&archive_pin.asset);
                        log.push(format!("downloading {url}"));
                        // The planned commands must equal the spawned
                        // actions: the downloads in this loop are exactly
                        // planned_commands()[0] and planned_commands()[1],
                        // in this order, when they are needed at all.
                        match ctx.ops.download(&url, &archive, &|_, _| {}).await {
                            Ok(bytes) => log.push(format!("downloaded {bytes} bytes")),
                            Err(e) => {
                                return Ok(StepOutcome::failed(
                                    format!("{e}"),
                                    vec![format!("{e:?}")],
                                ))
                            }
                        }
                        let actual = match ctx.ops.sha256_file(&archive).await {
                            Ok(h) => h,
                            Err(e) => {
                                return Ok(StepOutcome::failed(
                                    format!("{e}"),
                                    vec![format!("{e:?}")],
                                ))
                            }
                        };
                        if !checksums_match(&actual, &archive_pin.sha256) {
                            return Ok(StepOutcome::failed(
                                format!(
                                    "checksum mismatch for {}: expected {}, got {}",
                                    archive.display(),
                                    archive_pin.sha256,
                                    actual
                                ),
                                vec!["archive rejected; not installed".to_string()],
                            ));
                        }
                        log.push(format!("SHA-256 verified for {}", archive_pin.asset));
                    } else {
                        log.push(format!(
                            "{} already present with matching checksum",
                            archive_pin.asset
                        ));
                    }
                    if let Err(e) = ctx.ops.extract_zip(&archive, &dir).await {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                    }
                    log.push(format!(
                        "extracted {} to {}",
                        archive_pin.asset,
                        dir.display()
                    ));
                }
                let marker_json = serde_json::json!({
                    "tag": pin.tag,
                    "asset": pin.asset,
                    "sha256": pin.sha256,
                    "runtime_asset": pin.runtime.asset,
                    "runtime_sha256": pin.runtime.sha256,
                });
                if let Err(e) = ctx
                    .ops
                    .write_file(&marker, marker_json.to_string().as_bytes())
                    .await
                {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                }
                Ok(StepOutcome::done(
                    format!("llama.cpp {} installed to {}", pin.tag, dir.display()),
                    log,
                ))
            }
            RunMode::Audit => {
                // Re-verify without reinstalling: marker + BOTH archive
                // checksums.
                if !ctx.ops.file_exists(&marker).await {
                    return Ok(StepOutcome::failed(
                        "llama.cpp install marker missing",
                        vec![format!("{} not found", marker.display())],
                    ));
                }
                for archive_pin in &archives {
                    let archive = dir.join(&archive_pin.asset);
                    match ctx.ops.sha256_file(&archive).await {
                        Ok(h) if checksums_match(&h, &archive_pin.sha256) => {}
                        Ok(h) => {
                            return Ok(StepOutcome::failed(
                                format!(
                                    "installed build checksum mismatch for {}: expected {}, got {}",
                                    archive_pin.asset, archive_pin.sha256, h
                                ),
                                log,
                            ))
                        }
                        Err(e) => {
                            return Ok(StepOutcome::failed(
                                format!("cannot read installed archive {}: {e}", archive.display()),
                                vec![format!("{e:?}")],
                            ))
                        }
                    }
                }
                Ok(StepOutcome::done(
                    format!(
                        "llama.cpp {} build verified ({} + {})",
                        pin.tag, pin.asset, pin.runtime.asset
                    ),
                    log,
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Configure tensor-split
// ---------------------------------------------------------------------------

/// Compute per-GPU tensor-split fractions (hundredths, summing to exactly
/// 1.0), proportional to VRAM.
pub fn compute_splits(vram_mib: &[u64]) -> Result<Vec<f32>, WizardError> {
    let total: u64 = vram_mib.iter().sum();
    if total == 0 {
        return Err(WizardError::StepFailed(
            STEP_CONFIGURE_TENSOR_SPLIT.to_string(),
            "zero total VRAM across discovered GPUs".to_string(),
        ));
    }
    Ok(hundredths_of(
        &vram_mib.iter().map(|v| *v as f64).collect::<Vec<_>>(),
    ))
}

/// Split weights into hundredths fractions summing to exactly 1.0
/// (integer arithmetic; the rounding residue goes to the largest
/// share). Zero or negative total weight yields all-zero fractions.
fn hundredths_of(weights: &[f64]) -> Vec<f32> {
    let total: f64 = weights.iter().sum();
    if total <= 0.0 {
        return weights.iter().map(|_| 0.0).collect();
    }
    // Integer hundredths: exact sum, rounding residue goes to the largest share.
    let mut hundredths: Vec<i64> = weights
        .iter()
        .map(|v| ((*v / total) * 100.0).round() as i64)
        .collect();
    let residue = 100 - hundredths.iter().sum::<i64>();
    if let Some(idx) = hundredths
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.cmp(b.1))
        .map(|(i, _)| i)
    {
        hundredths[idx] += residue;
    }
    // Defensive: adversarial rounding on very large GPU counts can push a
    // share negative (each value rounds up by < 0.5, so the residue can be
    // as low as -N/2). Clamp negatives to zero and move the deficit onto
    // the positive shares, largest first; the sum stays exactly 100 and no
    // new negatives are created, so this terminates in one pass.
    let deficit: i64 = hundredths.iter().filter(|&&h| h < 0).map(|h| -h).sum();
    if deficit > 0 {
        for h in hundredths.iter_mut() {
            if *h < 0 {
                *h = 0;
            }
        }
        let mut order: Vec<usize> = (0..hundredths.len()).collect();
        order.sort_by(|&a, &b| hundredths[b].cmp(&hundredths[a]));
        let mut remaining = deficit;
        for &i in &order {
            if remaining == 0 {
                break;
            }
            let take = remaining.min(hundredths[i]);
            hundredths[i] -= take;
            remaining -= take;
        }
        debug_assert_eq!(
            remaining, 0,
            "positive shares always cover the deficit (they sum to 100 + deficit)"
        );
    }
    hundredths.iter().map(|h| *h as f32 / 100.0).collect()
}

/// One backend's input to [`assign_splits`].
pub struct SplitInput {
    /// Model size in bytes (the on-disk file when downloaded, else
    /// the params_b estimate). Zero when unknown — the entry then
    /// receives a VRAM-proportional split.
    pub model_bytes: u64,
    /// The backend's configured tensor split, when one is set.
    pub existing: Option<Vec<f32>>,
    /// True when the caller wants this entry (re)assigned if it has
    /// no valid split. False entries only account for their space.
    pub assign: bool,
}

/// Assign tensor splits across the discovered GPUs.
///
/// The defect this fixes (found on Pete's Precision, 2026-10-15):
/// backends shipped with no split at all, so llama.cpp placed every
/// model on the main GPU and overflowed to system RAM while the
/// second GPU held nothing but CUDA contexts.
///
/// Rules:
/// - An entry with a valid existing split (one fraction per GPU) is
///   kept as-is and its shares are deducted from the free pool. The
///   returned mode for a kept entry is informational only — callers
///   must not overwrite a kept entry's configured `split_mode`.
/// - An accounting-only entry (`assign: false`) with no valid split
///   deducts its whole model from GPU 0 — that is where llama.cpp
///   actually puts an unset backend today.
/// - Assignable entries are placed largest first: a model that fits
///   one GPU's remaining placeable space (90% of VRAM; the rest is
///   headroom for the KV cache, compute buffers, and the CUDA
///   context) goes entirely on the GPU with the most room, as a
///   one-hot split; a larger model is spread across GPUs in
///   proportion to their remaining room.
/// - A single-GPU machine gets no assignments: a split there is a
///   no-op and leaving it unset keeps the configuration honest.
pub fn assign_splits(
    inputs: &[SplitInput],
    gpu_vram_mib: &[u64],
) -> Vec<Option<(Vec<f32>, SplitMode)>> {
    let n = gpu_vram_mib.len();
    let mut out: Vec<Option<(Vec<f32>, SplitMode)>> = vec![None; inputs.len()];
    if n == 0 {
        return out;
    }
    let mut remaining: Vec<f64> = gpu_vram_mib
        .iter()
        .map(|v| *v as f64 * 1_048_576.0 * 0.90)
        .collect();
    // Pass 1: account for entries that will not be (re)assigned.
    for (i, input) in inputs.iter().enumerate() {
        match &input.existing {
            Some(split) if split.len() == n => {
                out[i] = Some((split.clone(), SplitMode::Layer));
                for (g, share) in split.iter().enumerate() {
                    remaining[g] =
                        (remaining[g] - *share as f64 * input.model_bytes as f64).max(0.0);
                }
            }
            _ if !input.assign => {
                remaining[0] = (remaining[0] - input.model_bytes as f64).max(0.0);
            }
            _ => {}
        }
    }
    if n == 1 {
        return out;
    }
    // Pass 2: assign the rest, largest first (stable for ties).
    let mut order: Vec<usize> = (0..inputs.len())
        .filter(|&i| out[i].is_none() && inputs[i].assign)
        .collect();
    order.sort_by(|&a, &b| inputs[b].model_bytes.cmp(&inputs[a].model_bytes));
    for i in order {
        let bytes = inputs[i].model_bytes as f64;
        // GPU with the most room; lowest index wins ties (GPU 0 is
        // the main GPU — prefer filling it first, deterministically).
        let mut g = 0usize;
        for cand in 1..n {
            if remaining[cand] > remaining[g] {
                g = cand;
            }
        }
        let room = remaining[g];
        if bytes > 0.0 && bytes <= room {
            let mut split = vec![0.0f32; n];
            split[g] = 1.0;
            remaining[g] -= bytes;
            out[i] = Some((split, SplitMode::Layer));
            continue;
        }
        // Does not fit on one GPU: spread over the room that remains,
        // falling back to raw capacity proportions when the pool is
        // exhausted (llama.cpp will spill the excess to system RAM,
        // as it does today — but distributed, not all on GPU 0).
        let weights: Vec<f64> = if remaining.iter().sum::<f64>() > 0.0 {
            remaining.clone()
        } else {
            gpu_vram_mib.iter().map(|v| *v as f64).collect()
        };
        let split = hundredths_of(&weights);
        for (g, share) in split.iter().enumerate() {
            remaining[g] = (remaining[g] - *share as f64 * bytes).max(0.0);
        }
        out[i] = Some((split, SplitMode::Layer));
    }
    out
}

/// Estimate a model's on-disk size from its parameter count, using
/// the catalog's derivation (~4.92 bits per weight for Q4_K_M plus
/// ~5% overhead) — the same estimate `placement_propose` uses in the
/// Tauri layer. Used only when the model file is not downloaded yet
/// (the tensor-split step runs before the download step).
pub fn estimate_model_bytes(params_b: f64) -> u64 {
    if params_b > 0.0 {
        (params_b * 1e9 * 4.92 / 8.0 * 1.05) as u64
    } else {
        0
    }
}

/// Step 3: compute per-backend tensor splits from discovered VRAM, write the
/// flag sets, validate sums to 1.0. Audit mode re-validates.
pub struct ConfigureTensorSplit;

fn fmt_split(split: &[f32]) -> String {
    split
        .iter()
        .map(|v| format!("{v:.2}"))
        .collect::<Vec<_>>()
        .join("/")
}

impl ConfigureTensorSplit {
    /// Size of the model a backend serves: the configured model whose
    /// gguf_path the backend's model_file resolves to. The on-disk
    /// length when the file is downloaded, else the params_b
    /// estimate (this step runs before the download step, so the
    /// estimate is the common case on a first run). Zero when the
    /// backend's model cannot be resolved — the assignment then
    /// falls back to a VRAM-proportional split.
    async fn model_bytes(&self, ctx: &WizardContext, backend: &BackendConfig) -> u64 {
        let target = resolve_dest(&ctx.data_dir, &backend.model_file);
        let model = ctx
            .config
            .models
            .iter()
            .find(|m| resolve_dest(&ctx.data_dir, &m.gguf_path) == target);
        match model {
            Some(m) => {
                let path = resolve_dest(&ctx.data_dir, &m.gguf_path);
                match ctx.ops.file_len(&path).await {
                    Some(len) => len,
                    None => estimate_model_bytes(m.params_b as f64),
                }
            }
            None => 0,
        }
    }

    async fn run_install(
        &self,
        ctx: &WizardContext,
        vrams: &[u64],
    ) -> Result<StepOutcome, WizardError> {
        let mut log: Vec<String> = Vec::new();
        let enabled: Vec<&BackendConfig> =
            ctx.config.backends.iter().filter(|b| b.enabled).collect();
        if enabled.is_empty() {
            return Ok(StepOutcome::done(
                "no enabled backends; nothing to place".to_string(),
                log,
            ));
        }
        if vrams.len() == 1 {
            return Ok(StepOutcome::done(
                "single GPU discovered; no tensor split needed — llama.cpp uses the only GPU"
                    .to_string(),
                log,
            ));
        }
        let mut inputs: Vec<SplitInput> = Vec::new();
        for b in &enabled {
            inputs.push(SplitInput {
                model_bytes: self.model_bytes(ctx, b).await,
                existing: if b.flags.tensor_split.is_empty() {
                    None
                } else {
                    Some(b.flags.tensor_split.clone())
                },
                assign: true,
            });
        }
        let assigned = assign_splits(&inputs, vrams);
        // Persist into the on-disk config (the pattern
        // DownloadModels::stamp_verified_at documents: steps receive
        // a snapshot clone, so placement must be written to
        // <data_dir>/config.json to exist at all; the app reloads the
        // file after each wizard step). Base: the persisted config
        // when it loads, else the context snapshot.
        let config_path = ctx.data_dir.join("config.json");
        let mut config = match manager_config::load(&config_path) {
            Ok(cfg) => cfg,
            Err(_) if ctx.data_dir.is_dir() => ctx.config.clone(),
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!("could not load the persisted config to write splits: {e}"),
                    log,
                ))
            }
        };
        let mut changed = 0usize;
        for (b, result) in enabled.iter().zip(assigned.iter()) {
            let Some((split, mode)) = result else {
                continue;
            };
            if b.flags.tensor_split == *split {
                log.push(format!(
                    "{}: kept existing split [{}]",
                    b.id,
                    fmt_split(split)
                ));
                continue;
            }
            if let Some(target) = config.backends.iter_mut().find(|x| x.id == b.id) {
                target.flags.tensor_split = split.clone();
                target.flags.split_mode = *mode;
            }
            changed += 1;
            log.push(format!(
                "{}: split [{}] assigned (model ~{} GB)",
                b.id,
                fmt_split(split),
                manager_present::gb(
                    inputs[enabled.iter().position(|x| x.id == b.id).unwrap_or(0)].model_bytes
                        as f64
                        / 1_073_741_824.0
                )
            ));
        }
        if changed > 0 {
            if let Err(e) = manager_config::validate(&config) {
                return Ok(StepOutcome::failed(
                    format!("computed splits failed validation: {e}"),
                    log,
                ));
            }
            if let Err(e) = manager_config::save(&config, &config_path) {
                return Ok(StepOutcome::failed(
                    format!("could not save splits to {}: {e}", config_path.display()),
                    log,
                ));
            }
            log.push(format!(
                "saved placement for {changed} backend(s) to {}",
                config_path.display()
            ));
        }
        Ok(StepOutcome::done(
            format!(
                "tensor splits set for {} enabled backend(s) across {} GPUs ({changed} updated)",
                enabled.len(),
                vrams.len()
            ),
            log,
        ))
    }

    fn run_audit(&self, ctx: &WizardContext, gpu_count: usize) -> StepOutcome {
        let mut log: Vec<String> = Vec::new();
        let mut problems: Vec<String> = Vec::new();
        let enabled: Vec<&BackendConfig> =
            ctx.config.backends.iter().filter(|b| b.enabled).collect();
        for b in &enabled {
            if gpu_count == 1 {
                log.push(format!("{}: single GPU, no split required", b.id));
                continue;
            }
            if b.flags.tensor_split.is_empty() {
                problems.push(format!("{}: no tensor split configured", b.id));
            } else if b.flags.tensor_split.len() != gpu_count {
                problems.push(format!(
                    "{}: split covers {} GPUs, {} discovered",
                    b.id,
                    b.flags.tensor_split.len(),
                    gpu_count
                ));
            } else {
                let sum: f32 = b.flags.tensor_split.iter().sum();
                if (sum - 1.0).abs() > 1e-3 {
                    problems.push(format!("{}: splits sum to {sum:.4}, not 1.0", b.id));
                } else {
                    log.push(format!(
                        "{}: split [{}] valid",
                        b.id,
                        fmt_split(&b.flags.tensor_split)
                    ));
                }
            }
        }
        if problems.is_empty() {
            StepOutcome::done(
                format!(
                    "tensor splits valid for {} enabled backend(s)",
                    enabled.len()
                ),
                log,
            )
        } else {
            for p in &problems {
                log.push(p.clone());
            }
            StepOutcome::failed(
                format!("tensor-split problems: {}", problems.join("; ")),
                log,
            )
        }
    }
}

#[async_trait]
impl WizardStep for ConfigureTensorSplit {
    fn id(&self) -> &str {
        STEP_CONFIGURE_TENSOR_SPLIT
    }
    fn name(&self) -> &str {
        "Configure tensor-split"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![
            STEP_DETECT_HARDWARE.to_string(),
            STEP_INSTALL_LLAMACPP.to_string(),
        ]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, _ctx: &WizardContext) -> Vec<PlannedCommand> {
        // Pure compute + config write; no external commands.
        vec![]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("re-run configure-tensor-split to recompute splits from current VRAM")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let gpus = match ctx.ops.gpu_inventory().await {
            Ok(g) => g,
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!("GPU probe failed: {e}"),
                    vec![format!("{e:?}")],
                ))
            }
        };
        if gpus.is_empty() {
            return Ok(StepOutcome::failed(
                "no GPUs discovered; cannot compute tensor splits".to_string(),
                vec![],
            ));
        }
        let vrams: Vec<u64> = gpus.iter().map(|g| g.total_vram_mib).collect();
        match ctx.mode {
            RunMode::Install => self.run_install(ctx, &vrams).await,
            RunMode::Audit => Ok(self.run_audit(ctx, gpus.len())),
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Download models + checksum verify
// ---------------------------------------------------------------------------

/// Step 4: download curated GGUFs with SHA-256 verification.
/// Audit mode re-verifies installed files without downloading.
///
/// Every model whose checksum matches during a run — downloaded,
/// already present, or audited — gets `verified_at` stamped into the
/// persisted config (see [`DownloadModels::stamp_verified_at`]), so
/// the app's fail-closed backend-start gate accepts wizard-verified
/// models without a second verification in the Models view.
///
/// Download URLs come from [`ModelConfig::source_url`](manager_config::ModelConfig)
/// (the curated catalog, spec §4.6); a model with an empty `source_url` fails
/// closed with a clear message.
pub struct DownloadModels;

#[async_trait]
impl WizardStep for DownloadModels {
    fn id(&self) -> &str {
        STEP_DOWNLOAD_MODELS
    }
    fn name(&self) -> &str {
        "Download models and verify checksums"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_INSTALL_LLAMACPP.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        ctx.config
            .models
            .iter()
            .filter(|m| m.enabled && !m.source_url.is_empty())
            .map(|m| {
                let dest = resolve_dest(&ctx.data_dir, &m.gguf_path);
                PlannedCommand::new(vec![
                    "download".to_string(),
                    m.source_url.clone(),
                    dest.display().to_string(),
                ])
            })
            .collect()
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("model checksum mismatch → re-download?")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        if ctx.config.models.is_empty() {
            return Ok(StepOutcome::done(
                "no models configured; nothing to download",
                vec![],
            ));
        }
        // Disabled models are out of scope for the wizard: their files
        // and configuration stay, but nothing downloads or verifies.
        let targets: Vec<&ModelConfig> = ctx.config.models.iter().filter(|m| m.enabled).collect();
        if targets.is_empty() {
            return Ok(StepOutcome::done(
                "no models enabled; nothing to download (enable models in the Models view)",
                vec![],
            ));
        }
        let mut log = Vec::new();
        let mut verified = 0;
        let mut unverified = 0;
        if ctx.mode == RunMode::Install {
            if let Err(outcome) = self.check_disk_space(ctx, &mut log).await {
                return Ok(outcome);
            }
        }
        for model in &targets {
            let dest = resolve_dest(&ctx.data_dir, &model.gguf_path);
            if model.source_url.is_empty() {
                return Ok(StepOutcome::failed(
                    format!("no source URL configured for model '{}'", model.id),
                    vec!["set models[].source_url in the config (spec §4.6)".to_string()],
                ));
            }
            let url = model.source_url.clone();
            // Only a pinned hash can verify. A model without one (a
            // user-added model with no recorded SHA-256) downloads and
            // runs unverified rather than failing a comparison that can
            // never succeed.
            let pinned = is_pinned_hash(&model.sha256);
            let verify = |actual: &str| pinned && checksums_match(actual, &model.sha256);
            match ctx.mode {
                RunMode::Install => {
                    let mut needs_download = true;
                    if ctx.ops.file_exists(&dest).await {
                        if !pinned {
                            log.push(format!(
                                "{}: already present (no pinned SHA-256; not verified)",
                                model.id
                            ));
                            needs_download = false;
                        } else {
                            match ctx.ops.sha256_file(&dest).await {
                                Ok(h) if verify(&h) => {
                                    log.push(format!("{}: already present and verified", model.id));
                                    needs_download = false;
                                }
                                _ => log.push(format!(
                                    "{}: present but checksum mismatch; re-downloading",
                                    model.id
                                )),
                            }
                        }
                    }
                    if needs_download {
                        log.push(format!("downloading {} → {}", url, dest.display()));
                        if let Err(e) = ctx.ops.download(&url, &dest, &|_, _| {}).await {
                            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                        }
                        if pinned {
                            let actual = match ctx.ops.sha256_file(&dest).await {
                                Ok(h) => h,
                                Err(e) => {
                                    return Ok(StepOutcome::failed(
                                        format!("{e}"),
                                        vec![format!("{e:?}")],
                                    ))
                                }
                            };
                            if !verify(&actual) {
                                return Ok(StepOutcome::failed(
                                    format!(
                                        "checksum mismatch for '{}': expected {}, got {}",
                                        model.id, model.sha256, actual
                                    ),
                                    vec!["downloaded file rejected".to_string()],
                                ));
                            }
                            log.push(format!("{}: downloaded and SHA-256 verified", model.id));
                        } else {
                            log.push(format!(
                                "{}: downloaded (no pinned SHA-256; not verified)",
                                model.id
                            ));
                        }
                    }
                    if pinned {
                        Self::stamp_verified_at(ctx, &model.id, &mut log);
                        verified += 1;
                    } else {
                        unverified += 1;
                    }
                }
                RunMode::Audit => {
                    if !ctx.ops.file_exists(&dest).await {
                        return Ok(StepOutcome::failed(
                            format!("model '{}' missing at {}", model.id, dest.display()),
                            vec![],
                        ));
                    }
                    if !pinned {
                        log.push(format!(
                            "{}: present (no pinned SHA-256; cannot verify)",
                            model.id
                        ));
                        unverified += 1;
                        continue;
                    }
                    match ctx.ops.sha256_file(&dest).await {
                        Ok(h) if verify(&h) => {
                            log.push(format!("{}: checksum verified", model.id));
                            Self::stamp_verified_at(ctx, &model.id, &mut log);
                            verified += 1;
                        }
                        Ok(h) => {
                            return Ok(StepOutcome::failed(
                                format!(
                                    "model '{}' checksum mismatch: expected {}, got {}",
                                    model.id, model.sha256, h
                                ),
                                log,
                            ));
                        }
                        Err(e) => {
                            return Ok(StepOutcome::failed(
                                format!("cannot read '{}': {e}", model.id),
                                vec![format!("{e:?}")],
                            ));
                        }
                    }
                }
            }
        }
        let mut message = format!("{verified}/{} model(s) verified", targets.len());
        if unverified > 0 {
            message.push_str(&format!(
                "; {unverified} present without a pinned SHA-256 (not verified)"
            ));
        }
        Ok(StepOutcome::done(message, log))
    }
}

impl DownloadModels {
    /// Stamp `verified_at` for one model this run just verified into
    /// the persisted config.
    ///
    /// Steps receive a snapshot clone of the app config
    /// (`WizardContext::config` is built fresh by the caller and steps
    /// run through `&WizardContext`), so the stamp cannot ride back
    /// through `ctx.config` — nothing a step does to it reaches the
    /// saved `AppConfig`. The persisted config lives at
    /// `<data_dir>/config.json` (the path the app loads at startup);
    /// it is the base when it loads. On a first run, when nothing has
    /// saved that file yet, the context config is the base instead —
    /// it is the caller's current config, cloned for this step run,
    /// so saving it back persists exactly what the app would save
    /// later, plus the stamp. When there is no real data dir on disk
    /// (unit tests drive the step with fake ops and a fictional data
    /// dir) there is no persisted config to stamp and this is a no-op.
    ///
    /// Best-effort: failures are logged, never failed into the step
    /// outcome. The verification itself already succeeded; an
    /// unstamped model simply stays gated until something else
    /// verifies it.
    fn stamp_verified_at(ctx: &WizardContext, model_id: &str, log: &mut Vec<String>) {
        let config_path = ctx.data_dir.join("config.json");
        let mut config = match manager_config::load(&config_path) {
            Ok(cfg) => cfg,
            Err(_) if ctx.data_dir.is_dir() => ctx.config.clone(),
            Err(_) => return,
        };
        let Some(model) = config.models.iter_mut().find(|m| m.id == model_id) else {
            return;
        };
        model.verified_at = Some(chrono::Utc::now());
        match manager_config::save(&config, &config_path) {
            Ok(()) => log.push(format!(
                "{model_id}: verified_at stamped in {}",
                config_path.display()
            )),
            Err(e) => log.push(format!(
                "{model_id}: could not stamp verified_at in {}: {e}",
                config_path.display()
            )),
        }
    }

    /// Fail when the models still needing download exceed the free space on
    /// the data volume (10% headroom). Sizes come from HEAD Content-Length;
    /// models whose size cannot be determined are logged, not counted. A
    /// failed free-space probe warns instead of blocking.
    async fn check_disk_space(
        &self,
        ctx: &WizardContext,
        log: &mut Vec<String>,
    ) -> Result<(), StepOutcome> {
        let mut known: u64 = 0;
        let mut unknown: u32 = 0;
        for model in &ctx.config.models {
            if !model.enabled || model.source_url.is_empty() {
                continue; // disabled models are out of scope; the per-model loop reports a missing URL
            }
            let dest = resolve_dest(&ctx.data_dir, &model.gguf_path);
            let needs_download = if ctx.ops.file_exists(&dest).await {
                // An unpinned model already on disk cannot be verified,
                // so it counts as present — same rule as the run loop.
                is_pinned_hash(&model.sha256)
                    && !matches!(ctx.ops.sha256_file(&dest).await, Ok(h) if checksums_match(&h, &model.sha256))
            } else {
                true
            };
            if !needs_download {
                continue;
            }
            match ctx.ops.http_head_content_length(&model.source_url).await {
                Ok(Some(n)) => {
                    known = known.saturating_add(n);
                    log.push(format!("{}: download size ~{n} bytes", model.id));
                }
                _ => {
                    unknown += 1;
                    log.push(format!(
                        "{}: size unknown, excluded from space check",
                        model.id
                    ));
                }
            }
        }
        if known == 0 {
            log.push(format!(
                "disk-space pre-check skipped: no download sizes known ({unknown} model(s) with unknown size)"
            ));
            return Ok(());
        }
        let free = match ctx.ops.free_space_bytes(&ctx.data_dir).await {
            Ok(f) => f,
            Err(e) => {
                log.push(format!(
                    "free-space probe failed ({e}); skipping space check"
                ));
                return Ok(());
            }
        };
        let needed = known + known / 10; // 10% headroom for temp files
        log.push(format!(
            "space check: need ~{}, {} free on {}",
            manager_present::bytes(needed),
            manager_present::bytes(free),
            ctx.data_dir.display()
        ));
        if free < needed {
            return Err(StepOutcome::failed(
                format!(
                    "insufficient disk space: need ~{} for model downloads, have {} free on {}",
                    manager_present::bytes(needed),
                    manager_present::bytes(free),
                    ctx.data_dir.display()
                ),
                vec![
                    "free up disk space or point data_dir at a larger volume, then re-run"
                        .to_string(),
                ],
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 5. App-managed PostgreSQL
// ---------------------------------------------------------------------------

/// The pinned PostgreSQL distribution (EnterpriseDB binaries zip).
/// The hash is self-computed from the downloaded artifact (Oct 15,
/// 2026): EDB publishes no official SHA-256 for these zips — the
/// same trust model as the llama.cpp pins.
#[derive(Debug, Clone)]
pub struct PostgresPin {
    pub version: String,
    pub asset: String,
    pub sha256: String,
    pub size: u64,
}

impl PostgresPin {
    pub fn default_pin() -> Self {
        Self {
            version: "18.6-1".to_string(),
            asset: "postgresql-18.6-1-windows-x64-binaries.zip".to_string(),
            sha256: "fbe23da234ee31547bf8a36d29dfd81e82b849df2d2b78d2eecb43d360252f8c".to_string(),
            size: 343_808_005,
        }
    }

    pub fn url(&self) -> String {
        format!("https://get.enterprisedb.com/postgresql/{}", self.asset)
    }
}

/// Root of the app-managed PostgreSQL install: `<data_dir>/postgres`.
pub fn postgres_root(data_dir: &Path) -> PathBuf {
    data_dir.join("postgres")
}

/// `<data_dir>/postgres/bin` — the extracted server binaries.
pub fn postgres_bin_dir(data_dir: &Path) -> PathBuf {
    postgres_root(data_dir).join("bin")
}

/// `<data_dir>/postgres/data` — the database cluster directory.
pub fn postgres_data_dir(data_dir: &Path) -> PathBuf {
    postgres_root(data_dir).join("data")
}

fn postgres_marker_path(data_dir: &Path) -> PathBuf {
    postgres_root(data_dir).join(".installed.json")
}

fn postgres_zip_path(data_dir: &Path, pin: &PostgresPin) -> PathBuf {
    postgres_root(data_dir).join("downloads").join(&pin.asset)
}

/// The server's own log file, in the app's logs directory alongside
/// the gateway logs (`pg_ctl -l` appends there).
pub fn postgres_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("postgres.log")
}

/// The database the gateway uses.
pub const POSTGRES_DATABASE: &str = "litellm";

/// The `DATABASE_URL` the gateway receives: the password is
/// generated hex (URL-safe by construction, no percent-encoding),
/// the user is the cluster superuser, the database is
/// [`POSTGRES_DATABASE`].
pub fn postgres_database_url(config: &AppConfig, password: &str) -> String {
    format!(
        "postgresql://postgres:{password}@127.0.0.1:{}/{POSTGRES_DATABASE}",
        config.postgres.port
    )
}

/// Whether the install-postgres step has completed: the server
/// binaries in place, and either the install marker or an
/// initialized cluster. The marker is bookkeeping the step writes
/// last; a lost marker must not strand a working install (the step
/// rewrites it on its next run), so the binaries plus the cluster
/// are accepted as the ground truth.
pub async fn postgres_installed(ops: &dyn SystemOps, data_dir: &Path) -> bool {
    if !ops
        .file_exists(&postgres_bin_dir(data_dir).join("pg_ctl.exe"))
        .await
    {
        return false;
    }
    ops.file_exists(&postgres_marker_path(data_dir)).await
        || ops
            .file_exists(&postgres_data_dir(data_dir).join("PG_VERSION"))
            .await
}

async fn postgres_ready(ops: &dyn SystemOps, config: &AppConfig, data_dir: &Path) -> bool {
    let cmd = PlannedCommand::new(vec![
        postgres_bin_dir(data_dir)
            .join("pg_isready.exe")
            .display()
            .to_string(),
        "-h".to_string(),
        "127.0.0.1".to_string(),
        "-p".to_string(),
        config.postgres.port.to_string(),
    ]);
    matches!(ops.run_command(&cmd).await, Ok(out) if out.exit_code == 0)
}

async fn postgres_log_tail(ops: &dyn SystemOps, data_dir: &Path) -> String {
    match ops.read_file(&postgres_log_path(data_dir)).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            let tail = &lines[lines.len().saturating_sub(15)..];
            tail.join("\n")
        }
        Err(_) => "(no server log yet)".to_string(),
    }
}

fn pg_ctl_command(data_dir: &Path, args: &[&str]) -> PlannedCommand {
    let mut argv = vec![
        postgres_bin_dir(data_dir)
            .join("pg_ctl.exe")
            .display()
            .to_string(),
        "-D".to_string(),
        postgres_data_dir(data_dir).display().to_string(),
    ];
    argv.extend(args.iter().map(|a| a.to_string()));
    PlannedCommand::new(argv)
}

/// Start the app-managed PostgreSQL server if it is not already
/// accepting connections, and return the gateway's `DATABASE_URL`.
/// The caller must have established [`postgres_installed`] first.
/// Failure carries the server log tail: the decisive case is
/// PostgreSQL's refusal to run under an administrative token, which
/// is recorded in that log verbatim.
pub async fn ensure_postgres_running(
    ops: &dyn SystemOps,
    config: &AppConfig,
    data_dir: &Path,
    log: &mut Vec<String>,
) -> Result<String, String> {
    let password = crate::ensure_postgres_password(ops)
        .await
        .map_err(|e| format!("could not obtain the PostgreSQL password: {e}"))?;
    let url = postgres_database_url(config, &password);
    if postgres_ready(ops, config, data_dir).await {
        log.push(format!(
            "PostgreSQL already running on 127.0.0.1:{}",
            config.postgres.port
        ));
        return Ok(url);
    }
    let port_opt = format!("-p {} -h 127.0.0.1", config.postgres.port);
    let log_arg = postgres_log_path(data_dir).display().to_string();
    let start = pg_ctl_command(data_dir, &["-l", &log_arg, "-o", &port_opt, "start"]);
    match ops.run_command(&start).await {
        Ok(out) if out.exit_code == 0 => {
            log.push(format!(
                "PostgreSQL started on 127.0.0.1:{} (log: {})",
                config.postgres.port,
                postgres_log_path(data_dir).display()
            ));
        }
        Ok(out) => {
            let tail = postgres_log_tail(ops, data_dir).await;
            return Err(format!(
                "PostgreSQL failed to start (pg_ctl exit {}): {} — server log tail:\n{tail}",
                out.exit_code,
                out.stderr.trim()
            ));
        }
        Err(e) => return Err(format!("PostgreSQL failed to start: {e}")),
    }
    // pg_ctl waits for readiness itself; poll pg_isready briefly for
    // the definitive signal.
    for _ in 0..30 {
        if postgres_ready(ops, config, data_dir).await {
            return Ok(url);
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let tail = postgres_log_tail(ops, data_dir).await;
    Err(format!(
        "PostgreSQL did not become ready on 127.0.0.1:{} — server log tail:\n{tail}",
        config.postgres.port
    ))
}

/// Stop the app-managed PostgreSQL server (`pg_ctl stop -m fast`:
/// rolls back in-flight transactions, disconnects clients,
/// checkpoints, exits). Best-effort semantics for shutdown paths:
/// Ok(true) when a running server was stopped, Ok(false) when it was
/// not installed or not running.
pub async fn stop_postgres_server(
    ops: &dyn SystemOps,
    data_dir: &Path,
) -> Result<bool, WizardError> {
    if !postgres_installed(ops, data_dir).await {
        return Ok(false);
    }
    let stop = pg_ctl_command(data_dir, &["stop", "-m", "fast"]);
    match ops.run_command(&stop).await {
        Ok(out) if out.exit_code == 0 => Ok(true),
        // "no server running" exits non-zero: the desired end state.
        Ok(_) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Step 5: install the pinned PostgreSQL binaries under the app
/// data dir, initialize the database cluster, and start the server
/// the LiteLLM gateway uses as its database (Pete's decision,
/// 2026-10-15: the LiteLLM admin UI cannot be logged into at all
/// without one — its schema is PostgreSQL-only). Only `bin`, `lib`,
/// `share`, and `include` are extracted from the distribution zip;
/// the rest (pgAdmin alone is ~684 MB) is dead weight.
pub struct InstallPostgres {
    pub pin: Option<PostgresPin>,
}

impl InstallPostgres {
    pub fn unconfigured() -> Self {
        Self { pin: None }
    }
    pub fn with_pin(pin: PostgresPin) -> Self {
        Self { pin: Some(pin) }
    }

    async fn binaries_present(&self, ctx: &WizardContext) -> bool {
        let bin = postgres_bin_dir(&ctx.data_dir);
        for exe in [
            "pg_ctl.exe",
            "initdb.exe",
            "postgres.exe",
            "psql.exe",
            "createdb.exe",
        ] {
            if !ctx.ops.file_exists(&bin.join(exe)).await {
                return false;
            }
        }
        true
    }

    async fn cluster_initialized(&self, ctx: &WizardContext) -> bool {
        // PG_VERSION is written by initdb and identifies a cluster.
        ctx.ops
            .file_exists(&postgres_data_dir(&ctx.data_dir).join("PG_VERSION"))
            .await
    }

    fn initdb_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            postgres_bin_dir(&ctx.data_dir)
                .join("initdb.exe")
                .display()
                .to_string(),
            "-D".to_string(),
            postgres_data_dir(&ctx.data_dir).display().to_string(),
            "-U".to_string(),
            "postgres".to_string(),
            "-A".to_string(),
            "scram-sha-256".to_string(),
            "--pwfile".to_string(),
            postgres_root(&ctx.data_dir)
                .join("pwfile.tmp")
                .display()
                .to_string(),
            "-E".to_string(),
            "UTF8".to_string(),
        ])
    }

    fn createdb_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            postgres_bin_dir(&ctx.data_dir)
                .join("createdb.exe")
                .display()
                .to_string(),
            "-h".to_string(),
            "127.0.0.1".to_string(),
            "-p".to_string(),
            ctx.config.postgres.port.to_string(),
            "-U".to_string(),
            "postgres".to_string(),
            POSTGRES_DATABASE.to_string(),
        ])
    }
}

#[async_trait]
impl WizardStep for InstallPostgres {
    fn id(&self) -> &str {
        STEP_INSTALL_POSTGRES
    }
    fn name(&self) -> &str {
        "Install PostgreSQL (gateway database)"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_DETECT_HARDWARE.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        let Some(pin) = self.pin.as_ref() else {
            return Vec::new();
        };
        let port_opt = format!("-p {} -h 127.0.0.1", ctx.config.postgres.port);
        let log_arg = postgres_log_path(&ctx.data_dir).display().to_string();
        vec![
            PlannedCommand::new(vec![
                "download".to_string(),
                pin.url(),
                postgres_zip_path(&ctx.data_dir, pin).display().to_string(),
            ]),
            self.initdb_command(ctx),
            pg_ctl_command(&ctx.data_dir, &["-l", &log_arg, "-o", &port_opt, "start"]),
            self.createdb_command(ctx),
        ]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("re-run install-postgres; if the server refuses to start, check the PostgreSQL log under the data dir's logs folder (running the manager itself elevated prevents PostgreSQL from starting)")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let pin = match &self.pin {
            Some(p) => p.clone(),
            None => {
                return Ok(StepOutcome::failed(
                    "no PostgreSQL build pinned",
                    vec![
                        "this build ships without a built-in PostgreSQL pin; that is a packaging error in the app, not a configuration problem".to_string(),
                        "failing closed: will not download an unpinned database server".to_string(),
                    ],
                ))
            }
        };
        let mut log = vec![format!(
            "pinned PostgreSQL: {} ({})",
            pin.version, pin.asset
        )];

        match ctx.mode {
            RunMode::Audit => {
                let mut problems = Vec::new();
                if !ctx
                    .ops
                    .file_exists(&postgres_marker_path(&ctx.data_dir))
                    .await
                {
                    problems.push(
                        "PostgreSQL not installed; run the install-postgres step".to_string(),
                    );
                }
                if !self.binaries_present(ctx).await {
                    problems.push("PostgreSQL binaries missing or incomplete".to_string());
                }
                if !self.cluster_initialized(ctx).await {
                    problems.push("database cluster not initialized".to_string());
                }
                if problems.is_empty() {
                    return Ok(StepOutcome::done(
                        format!(
                            "PostgreSQL {} verified (binaries and cluster present)",
                            pin.version
                        ),
                        log,
                    ));
                }
                return Ok(StepOutcome::failed(
                    format!("PostgreSQL setup incomplete: {}", problems.join("; ")),
                    log,
                ));
            }
            RunMode::Install => {}
        }

        // Binaries: download + verify + selective extraction.
        // Skipped when the binaries are in place and either the
        // marker or an initialized cluster proves the install
        // completed — re-extracting over an installed tree is what
        // locked-DLL failures come from (see the stop below).
        if self.binaries_present(ctx).await
            && (ctx
                .ops
                .file_exists(&postgres_marker_path(&ctx.data_dir))
                .await
                || self.cluster_initialized(ctx).await)
        {
            log.push("PostgreSQL binaries already installed".to_string());
        } else {
            let zip = postgres_zip_path(&ctx.data_dir, &pin);
            let zip_ok = ctx.ops.file_exists(&zip).await
                && matches!(ctx.ops.sha256_file(&zip).await, Ok(h) if checksums_match(&h, &pin.sha256));
            if !zip_ok {
                let url = pin.url();
                log.push(format!("downloading {url}"));
                match ctx.ops.download(&url, &zip, &|_, _| {}).await {
                    Ok(bytes) => log.push(format!("downloaded {bytes} bytes")),
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                }
                let actual = match ctx.ops.sha256_file(&zip).await {
                    Ok(h) => h,
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                };
                if !checksums_match(&actual, &pin.sha256) {
                    return Ok(StepOutcome::failed(
                        format!(
                            "checksum mismatch for {}: expected {}, got {}",
                            zip.display(),
                            pin.sha256,
                            actual
                        ),
                        vec!["distribution rejected; not installed".to_string()],
                    ));
                }
                log.push(format!("SHA-256 verified for {}", pin.asset));
            } else {
                log.push(format!(
                    "{} already present with matching checksum",
                    pin.asset
                ));
            }
            // A running server holds its DLLs open; extracting over
            // them fails with a sharing violation (os error 32).
            // pg_isready is the ground truth for "a server is up" —
            // stop it first; the flow below restarts it via
            // ensure_postgres_running.
            if postgres_ready(ctx.ops.as_ref(), &ctx.config, &ctx.data_dir).await {
                log.push(
                    "PostgreSQL is running; stopping it before replacing its binaries (it restarts below)"
                        .to_string(),
                );
                let stop = pg_ctl_command(&ctx.data_dir, &["stop", "-m", "fast"]);
                match ctx.ops.run_command(&stop).await {
                    Ok(out) if out.exit_code == 0 => {}
                    Ok(out) => {
                        return Ok(StepOutcome::failed(
                            format!(
                                "could not stop the running PostgreSQL server before replacing its binaries (pg_ctl exit {}): {} — stop the gateway and re-run this step",
                                out.exit_code,
                                out.stderr.trim()
                            ),
                            log,
                        ));
                    }
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                }
            }
            if let Err(e) = ctx
                .ops
                .extract_zip_prefixes(
                    &zip,
                    &postgres_root(&ctx.data_dir),
                    &["pgsql/bin/", "pgsql/lib/", "pgsql/share/", "pgsql/include/"],
                    "pgsql/",
                )
                .await
            {
                return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
            }
            log.push(format!(
                "extracted bin/lib/share/include to {}",
                postgres_root(&ctx.data_dir).display()
            ));
            if !self.binaries_present(ctx).await {
                return Ok(StepOutcome::failed(
                    "extraction did not produce the pinned PostgreSQL binaries",
                    vec![
                        format!("expected pg_ctl.exe, initdb.exe, postgres.exe, psql.exe, createdb.exe under {}", postgres_bin_dir(&ctx.data_dir).display()),
                        "the distribution verified against its pin, so its layout differs from the pinned build's — failing closed".to_string(),
                    ],
                ));
            }
        }

        // The database password: generated once, stored in
        // Credential Manager, reused forever after (the cluster is
        // initialized with it; rotating it would lock the gateway
        // out of its own database).
        let password = match crate::ensure_postgres_password(ctx.ops.as_ref()).await {
            Ok(p) => p,
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!(
                    "could not store the PostgreSQL password in Windows Credential Manager: {e}"
                ),
                    vec![format!("{e:?}")],
                ))
            }
        };

        // Cluster initialization (once).
        if self.cluster_initialized(ctx).await {
            log.push("database cluster already initialized".to_string());
        } else {
            let pwfile = postgres_root(&ctx.data_dir).join("pwfile.tmp");
            if let Err(e) = ctx
                .ops
                .write_file(&pwfile, format!("{password}\n").as_bytes())
                .await
            {
                return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
            }
            let initdb = self.initdb_command(ctx);
            let result = ctx.ops.run_command(&initdb).await;
            // The password file is single-use: always remove it.
            let _ = ctx.ops.remove_file(&pwfile).await;
            match result {
                Ok(out) if out.exit_code == 0 => {
                    log.push(
                        "database cluster initialized (superuser postgres, scram-sha-256, UTF8)"
                            .to_string(),
                    );
                }
                Ok(out) => {
                    return Ok(StepOutcome::failed(
                        format!(
                            "initdb failed (exit {}): {}",
                            out.exit_code,
                            out.stderr.trim()
                        ),
                        {
                            let mut l = log.clone();
                            l.push(out.stdout.clone());
                            l
                        },
                    ));
                }
                Err(e) => return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")])),
            }
        }

        // Start the server and create the gateway's database.
        if let Err(e) =
            ensure_postgres_running(ctx.ops.as_ref(), &ctx.config, &ctx.data_dir, &mut log).await
        {
            return Ok(StepOutcome::failed(e, log));
        }
        let mut createdb = self.createdb_command(ctx);
        createdb.env.insert("PGPASSWORD".to_string(), password);
        match ctx.ops.run_command(&createdb).await {
            Ok(out) if out.exit_code == 0 => {
                log.push(format!("database {POSTGRES_DATABASE} created"));
            }
            Ok(out) if out.stderr.contains("already exists") => {
                log.push(format!("database {POSTGRES_DATABASE} already exists"));
            }
            Ok(out) => {
                return Ok(StepOutcome::failed(
                    format!(
                        "createdb failed (exit {}): {}",
                        out.exit_code,
                        out.stderr.trim()
                    ),
                    log,
                ));
            }
            Err(e) => return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")])),
        }

        let marker = serde_json::json!({
            "version": pin.version,
            "sha256": pin.sha256,
            "port": ctx.config.postgres.port,
            "database": POSTGRES_DATABASE,
            "at": chrono::Utc::now().to_rfc3339(),
        });
        if let Err(e) = ctx
            .ops
            .write_file(
                &postgres_marker_path(&ctx.data_dir),
                json_pretty(&marker).as_bytes(),
            )
            .await
        {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }
        Ok(StepOutcome::done(
            format!(
                "PostgreSQL {} running on 127.0.0.1:{} (database {POSTGRES_DATABASE})",
                pin.version, ctx.config.postgres.port
            ),
            log,
        ))
    }
}

/// The address the app itself uses to reach the gateway: loopback
/// whenever the bind covers it (127.0.0.1 and the all-interfaces
/// 0.0.0.0 / :: forms), otherwise the configured bind address
/// itself. Probing through loopback keeps health checks working
/// regardless of LAN state.
pub fn gateway_probe_host(config: &AppConfig) -> String {
    match config.gateway.bind_host.as_str() {
        "0.0.0.0" | "::" => "127.0.0.1".to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// 6. Start LiteLLM gateway
// ---------------------------------------------------------------------------

/// Step 6: bootstrap the managed Python env (uv), generate `config.yaml`
/// from the app config, spawn LiteLLM on :4000, health-check.
pub struct StartGateway {
    pub health_retries: u32,
    pub health_interval_ms: u64,
}

impl Default for StartGateway {
    fn default() -> Self {
        Self {
            // Two minutes: a cold LiteLLM start (interpreter + full
            // dependency import on a fresh venv, under antivirus
            // scanning) routinely exceeds the previous 30 s budget.
            // A process that exits is detected within one interval by
            // the liveness check in wait_healthy, so the larger budget
            // costs nothing on real failures.
            health_retries: 120,
            health_interval_ms: 1000,
        }
    }
}

impl StartGateway {
    fn venv_dir(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("python").join("venv")
    }

    /// Directory holding the bootstrapped uv binary.
    fn uv_bin_dir(ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("bin")
    }

    fn uv_binary_name() -> &'static str {
        if cfg!(target_os = "windows") {
            "uv.exe"
        } else {
            "uv"
        }
    }

    fn bundled_uv(ctx: &WizardContext) -> PathBuf {
        Self::uv_bin_dir(ctx).join(Self::uv_binary_name())
    }

    /// Find the uv binary in `dirs`. Pure and unit-testable.
    fn find_uv_in(dirs: &[PathBuf]) -> Option<PathBuf> {
        let name = Self::uv_binary_name();
        dirs.iter().map(|d| d.join(name)).find(|p| p.is_file())
    }

    /// `uv` on the real process PATH.
    fn uv_on_path() -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        Self::find_uv_in(&dirs)
    }

    /// Synchronously resolve the uv argv0 for display, plus whether run()
    /// will need to bootstrap it first.
    fn uv_argv0(ctx: &WizardContext) -> (PathBuf, bool) {
        if let Some(p) = Self::uv_on_path() {
            return (p, false);
        }
        let bundled = Self::bundled_uv(ctx);
        if bundled.is_file() {
            return (bundled, false);
        }
        // Not resolvable synchronously: run() bootstraps it on Windows;
        // elsewhere the bare name documents the PATH requirement.
        (bundled, crate::uv_download_url().is_some())
    }

    fn uv_archive_name() -> String {
        format!("uv-{}.zip", crate::UV_PINNED_VERSION)
    }

    /// Resolve a usable `uv`: PATH first, then the bootstrapped copy under
    /// data_dir/bin, else download + extract the pinned standalone build
    /// (Windows only; other platforms fail with install guidance).
    async fn ensure_uv(&self, ctx: &WizardContext) -> Result<PathBuf, StepOutcome> {
        if let Some(p) = Self::uv_on_path() {
            return Ok(p);
        }
        let bundled = Self::bundled_uv(ctx);
        if ctx.ops.file_exists(&bundled).await {
            return Ok(bundled);
        }
        let url = match crate::uv_download_url() {
            Some(u) => u,
            None => {
                return Err(StepOutcome::failed(
                    "uv not found on PATH and auto-bootstrap is only supported on Windows",
                    vec![format!(
                        "install uv from {} and re-run this step",
                        crate::UV_INSTALL_DOCS_URL
                    )],
                ))
            }
        };
        let dir = Self::uv_bin_dir(ctx);
        let archive = dir.join(Self::uv_archive_name());
        ctx.ops
            .download(&url, &archive, &|_, _| {})
            .await
            .map_err(|e| {
                StepOutcome::failed(
                    format!("uv bootstrap download failed: {e}"),
                    vec![format!("{e:?}")],
                )
            })?;
        ctx.ops.extract_zip(&archive, &dir).await.map_err(|e| {
            StepOutcome::failed(
                format!("uv bootstrap extraction failed: {e}"),
                vec![format!("{e:?}")],
            )
        })?;
        Ok(bundled)
    }

    fn venv_python(&self, ctx: &WizardContext) -> PathBuf {
        let venv = self.venv_dir(ctx);
        if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        }
    }

    fn gateway_url(&self, ctx: &WizardContext) -> String {
        format!(
            "http://{}:{}/v1/models",
            gateway_probe_host(&ctx.config),
            ctx.config.gateway.port
        )
    }

    fn litellm_spec(&self, ctx: &WizardContext) -> String {
        match ctx
            .config
            .updates
            .components
            .get("litellm")
            .and_then(|c| c.pinned_version.as_deref())
        {
            Some(v) => format!("litellm[proxy]=={v}"),
            None => "litellm[proxy]".to_string(),
        }
    }

    fn venv_command(&self, ctx: &WizardContext, uv: &std::path::Path) -> PlannedCommand {
        // --python pins the interpreter (see GATEWAY_PYTHON_VERSION);
        // --clear makes re-runs idempotent: a venv left behind by a
        // failed install or built with the wrong Python is replaced
        // instead of failing with "already exists".
        PlannedCommand::new(vec![
            uv.display().to_string(),
            "venv".to_string(),
            "--python".to_string(),
            crate::GATEWAY_PYTHON_VERSION.to_string(),
            "--clear".to_string(),
            self.venv_dir(ctx).display().to_string(),
        ])
    }

    /// Probe printing the venv interpreter's "major.minor" version.
    fn version_probe_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            self.venv_python(ctx).display().to_string(),
            "-c".to_string(),
            "import sys; print(f'{sys.version_info[0]}.{sys.version_info[1]}')".to_string(),
        ])
    }

    /// Probe that succeeds only when litellm imports in the venv.
    fn litellm_probe_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            self.venv_python(ctx).display().to_string(),
            "-c".to_string(),
            // prisma and prometheus_client ride along: a venv that
            // predates either addition to the install spec (see
            // pip_command) must not pass this probe, so the pip step
            // runs and completes it.
            "import litellm, prisma, prometheus_client".to_string(),
        ])
    }

    /// The existing venv interpreter's "major.minor" version, or None
    /// when there is no venv interpreter or it cannot report one.
    async fn venv_python_version(&self, ctx: &WizardContext) -> Option<String> {
        let python = self.venv_python(ctx);
        if !ctx.ops.file_exists(&python).await {
            return None;
        }
        let out = ctx
            .ops
            .run_command(&self.version_probe_command(ctx))
            .await
            .ok()?;
        if !out.success() {
            return None;
        }
        let v = out.stdout.trim().to_string();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }

    /// Whether litellm already imports in the venv.
    async fn litellm_importable(&self, ctx: &WizardContext) -> bool {
        matches!(
            ctx.ops.run_command(&self.litellm_probe_command(ctx)).await,
            Ok(out) if out.success()
        )
    }

    fn pip_command(&self, ctx: &WizardContext, uv: &std::path::Path) -> PlannedCommand {
        PlannedCommand::new(vec![
            uv.display().to_string(),
            "pip".to_string(),
            "install".to_string(),
            "--python".to_string(),
            self.venv_python(ctx).display().to_string(),
            self.litellm_spec(ctx),
            // prisma is not part of litellm[proxy] (it sits in the
            // separate extra-proxy extra), but the proxy's auth
            // exception handler imports it unconditionally when
            // classifying an authentication error — without it an
            // unauthenticated request returns HTTP 500 (the handler
            // itself crashes on ModuleNotFoundError) instead of a
            // clean 401. Found on real hardware 2026-10-15.
            "prisma".to_string(),
            // prometheus-client is only in litellm's proxy-runtime
            // extra. The generated gateway config enables the
            // prometheus success callback (dashboard request/error
            // stats), and LiteLLM fails to start when the callback
            // is set but this package is missing — the mount is
            // unguarded during config load. Verified 2026-10-15.
            "prometheus-client".to_string(),
        ])
    }

    /// The venv's Python interpreter, used to run the gateway.
    ///
    /// The gateway is NOT spawned through the venv's `litellm.exe`
    /// console script. That file is a launcher (uv/pip trampoline):
    /// it spawns the real python.exe as its own child, forwarding
    /// neither our CREATE_NO_WINDOW flag nor our console — and
    /// because the launcher itself has no console, Windows allocates
    /// a fresh console window for python.exe, which stays on the
    /// desktop for the gateway's whole lifetime (found on real
    /// hardware 2026-10-15; the same indirection also made the
    /// recorded gateway pid the launcher's, not the server's).
    /// Spawning the interpreter directly keeps one process level:
    /// the creation flags hold, no window appears, and the pid is
    /// the server itself.
    ///
    /// On Windows pythonw.exe (the GUI-subsystem interpreter) is
    /// preferred when the venv provides one: it cannot own a console
    /// window at all. Stdio is always redirected to the gateway log
    /// files, which pythonw requires.
    fn gateway_python(&self, ctx: &WizardContext) -> PathBuf {
        let venv = self.venv_dir(ctx);
        if cfg!(windows) {
            let windowless = venv.join("Scripts").join("pythonw.exe");
            if windowless.exists() {
                return windowless;
            }
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        }
    }

    /// The exact bootstrap the venv's `litellm` console script runs
    /// (verified against the litellm 1.105.1 wheel's entry_points.txt:
    /// `litellm = litellm:run_server`, a click command that parses
    /// sys.argv[1:] when called with no arguments). Running it via
    /// `-c` invokes the same entry point with our arguments in
    /// sys.argv[1:], without the launcher process in between.
    /// `python -m litellm` is not an alternative — the package ships
    /// no `__main__` and exits immediately (found on real hardware
    /// 2026-10-15).
    const GATEWAY_BOOTSTRAP: &str =
        "import sys; from litellm import run_server; sys.exit(run_server())";

    /// The Windows firewall rule that admits LAN clients to the
    /// gateway: inbound TCP on the gateway port, Private profile
    /// only, remote addresses limited to the local subnet.
    fn firewall_rule_name(ctx: &WizardContext) -> String {
        format!("Local LLM Gateway (TCP {})", ctx.config.gateway.port)
    }

    fn firewall_show_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            "netsh".to_string(),
            "advfirewall".to_string(),
            "firewall".to_string(),
            "show".to_string(),
            "rule".to_string(),
            format!("name={}", Self::firewall_rule_name(ctx)),
        ])
    }

    fn firewall_script_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("gateway").join("firewall.cmd")
    }

    fn firewall_elevated_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            self.firewall_script_path(ctx).display().to_string(),
        ])
    }

    fn firewall_script_contents(&self, ctx: &WizardContext, add: bool) -> String {
        let name = Self::firewall_rule_name(ctx);
        if add {
            format!(
                "@echo off\r\nnetsh advfirewall firewall delete rule name=\"{name}\" >nul 2>&1\r\nnetsh advfirewall firewall add rule name=\"{name}\" dir=in action=allow protocol=TCP localport={} profile=private remoteip=localsubnet\r\n",
                ctx.config.gateway.port
            )
        } else {
            format!("@echo off\r\nnetsh advfirewall firewall delete rule name=\"{name}\"\r\n")
        }
    }

    async fn firewall_rule_exists(&self, ctx: &WizardContext) -> bool {
        matches!(
            ctx.ops.run_command(&self.firewall_show_command(ctx)).await,
            Ok(out) if out.exit_code == 0
        )
    }

    /// Reconcile the firewall rule with the bind configuration: a
    /// LAN bind needs the rule, a loopback bind removes it. The
    /// rule query is unprivileged, so a matching state costs no UAC
    /// prompt; a mismatch runs one elevated script. A declined or
    /// failed elevation is a warning, never a step failure — the
    /// gateway itself is already healthy, and loopback clients are
    /// unaffected by the rule either way.
    async fn ensure_gateway_firewall(
        &self,
        ctx: &WizardContext,
        log: &mut Vec<String>,
    ) -> Option<String> {
        let want_rule = ctx.config.gateway.bind_host != "127.0.0.1";
        let have_rule = self.firewall_rule_exists(ctx).await;
        if want_rule == have_rule {
            log.push(if have_rule {
                "Windows firewall rule for LAN gateway access already present".to_string()
            } else {
                "gateway binds loopback only; no firewall rule needed".to_string()
            });
            return None;
        }
        let script = self.firewall_script_path(ctx);
        if let Err(e) = ctx
            .ops
            .write_file(
                &script,
                self.firewall_script_contents(ctx, want_rule).as_bytes(),
            )
            .await
        {
            return Some(format!(
                "could not write the firewall script {}: {e}",
                script.display()
            ));
        }
        match ctx
            .ops
            .run_elevated(&self.firewall_elevated_command(ctx))
            .await
        {
            Ok(_) => {
                log.push(if want_rule {
                    format!(
                        "Windows firewall rule added: inbound TCP {} (Private profile, local subnet)",
                        ctx.config.gateway.port
                    )
                } else {
                    "Windows firewall rule for LAN gateway access removed".to_string()
                });
                None
            }
            Err(e) => Some(format!(
                "the Windows firewall rule was not {} ({e}); {}",
                if want_rule { "added" } else { "removed" },
                if want_rule {
                    "clients on the local network may be unable to connect until the rule exists"
                } else {
                    "a stale allow rule for the gateway port remains, inert while the gateway binds loopback"
                }
            )),
        }
    }

    fn spawn_command(&self, ctx: &WizardContext) -> PlannedCommand {
        let cfg = ctx.data_dir.join("gateway").join("config.yaml");
        let mut cmd = PlannedCommand::new(vec![
            self.gateway_python(ctx).display().to_string(),
            "-c".to_string(),
            Self::GATEWAY_BOOTSTRAP.to_string(),
            "--config".to_string(),
            cfg.display().to_string(),
            // The configured bind address (default 0.0.0.0 — Pete's
            // decision 2026-10-15: other machines on the local
            // network use the gateway; 127.0.0.1 restricts it to this
            // computer). Authentication is the master key on every
            // interface.
            "--host".to_string(),
            ctx.config.gateway.bind_host.clone(),
            "--port".to_string(),
            ctx.config.gateway.port.to_string(),
        ]);
        // Force UTF-8 stdio in the gateway process. With stdout
        // redirected (the captured log file), Python on Windows
        // encodes output with the ANSI code page (cp1252); LiteLLM's
        // startup banner contains box-drawing characters cp1252
        // cannot represent, the print raises UnicodeEncodeError, and
        // application startup fails (found on real hardware
        // 2026-10-15 — a manual console run worked because a real
        // console takes the Unicode path). PYTHONIOENCODING targets
        // stdio; PYTHONUTF8 also covers the filesystem encoding.
        cmd.env
            .insert("PYTHONIOENCODING".to_string(), "utf-8".to_string());
        cmd.env.insert("PYTHONUTF8".to_string(), "1".to_string());
        cmd
    }

    /// The spawn command plus the OpenRouter key environment. The
    /// generated YAML references `os.environ/OPENROUTER_API_KEY`; the key
    /// itself lives in Windows Credential Manager under the tier's
    /// `cred_ref` and is read here — it is never logged and never
    /// written into the YAML. When the tier is enabled but the key
    /// cannot be read, the gateway still starts and the returned warning
    /// states the OpenRouter fallback is not armed.
    /// The gateway master key, generating and storing one on first
    /// use. Stored in Windows Credential Manager under
    /// [`crate::LITELLM_MASTER_KEY_CRED_TARGET`]; an existing key is
    /// always reused, so clients already configured with it (editor
    /// configs the wizard wrote) keep working across restarts.
    async fn ensure_master_key(&self, ctx: &WizardContext) -> Result<String, StepOutcome> {
        crate::ensure_gateway_master_key(ctx.ops.as_ref())
            .await
            .map_err(|e| {
                StepOutcome::failed(
                    format!(
                        "could not store the gateway master key in Windows Credential Manager: {e}"
                    ),
                    vec![format!("{e:?}")],
                )
            })
    }

    /// The master key for authenticating this app's own calls to the
    /// gateway (health probes, smoke tests), when one is stored.
    /// Never generates: probe paths must not create credentials.
    async fn stored_master_key(&self, ctx: &WizardContext) -> Option<String> {
        ctx.ops
            .credential_read(crate::LITELLM_MASTER_KEY_CRED_TARGET)
            .await
            .filter(|k| !k.is_empty())
    }

    /// The Authorization header for this app's own gateway calls,
    /// when a master key is stored. A gateway that requires a key was
    /// started by a run of this step, which stored the key first, so
    /// the header is present exactly when the gateway demands it.
    async fn gateway_auth_headers(&self, ctx: &WizardContext) -> Vec<(String, String)> {
        match self.stored_master_key(ctx).await {
            Some(key) => vec![("Authorization".to_string(), format!("Bearer {key}"))],
            None => Vec::new(),
        }
    }

    async fn spawn_command_with_env(
        &self,
        ctx: &WizardContext,
        database_url: Option<&str>,
    ) -> Result<(PlannedCommand, Option<String>), StepOutcome> {
        let mut cmd = self.spawn_command(ctx);
        // The master key is unconditional: current LiteLLM refuses
        // to boot without one. Failing to obtain it fails the step
        // here rather than spawning a gateway that exits at startup.
        let master_key = self.ensure_master_key(ctx).await?;
        cmd.env.insert("LITELLM_MASTER_KEY".to_string(), master_key);
        // The database URL is present exactly when the app-managed
        // PostgreSQL is installed and running (the caller started
        // it). With it, LiteLLM migrates its schema at startup and
        // the admin UI can be logged into; without it the gateway
        // runs database-free, as before PostgreSQL support.
        if let Some(url) = database_url {
            cmd.env.insert("DATABASE_URL".to_string(), url.to_string());
        }
        let tier = &ctx.config.gateway.openrouter;
        if !tier.enabled {
            return Ok((cmd, None));
        }
        let Some(cred_ref) = tier.cred_ref.as_deref().filter(|r| !r.is_empty()) else {
            return Ok((
                cmd,
                Some(
                    "OpenRouter fallback is not armed: the cloud tier is enabled but no credential reference is configured"
                        .to_string(),
                ),
            ));
        };
        match ctx.ops.credential_read(cred_ref).await {
            Some(key) => {
                cmd.env.insert("OPENROUTER_API_KEY".to_string(), key);
                Ok((cmd, None))
            }
            None => Ok((
                cmd,
                Some(format!(
                    "OpenRouter fallback is not armed: credential '{cred_ref}' could not be read from Windows Credential Manager"
                )),
            )),
        }
    }

    /// Stop the gateway recorded in `<data_dir>/gateway.pid`, if any.
    ///
    /// Restart semantics: the previously spawned LiteLLM still holds
    /// the port and serves the old generated config, so spawning a
    /// second one would fail to bind (silently — its stdio is nulled)
    /// while the health check passes against the old process. Stopping
    /// the recorded pid first is what makes re-running this step a
    /// real restart. A live recorded process that cannot be stopped
    /// fails the step: failing loudly beats a duplicate spawn.
    async fn stop_previous_gateway(
        &self,
        ctx: &WizardContext,
        log: &mut Vec<String>,
    ) -> Result<(), StepOutcome> {
        let pid_path = crate::gateway_pid_path(&ctx.data_dir);
        let recorded = match ctx.ops.read_file(&pid_path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).trim().parse::<u32>().ok(),
            Err(_) => None,
        };
        let Some(old_pid) = recorded else {
            return Ok(());
        };
        if ctx.ops.process_alive(old_pid) {
            if let Err(e) = ctx.ops.kill_process(old_pid) {
                return Err(StepOutcome::failed(
                    format!(
                        "could not stop the previous gateway (pid {old_pid}): {e}; \
                         refusing to spawn a duplicate on :{}",
                        ctx.config.gateway.port
                    ),
                    log.clone(),
                ));
            }
            log.push(format!("stopped previous gateway (pid {old_pid})"));
            // Wait briefly for the process to actually exit so the
            // port is free before the replacement spawns.
            for _ in 0..30 {
                if !ctx.ops.process_alive(old_pid) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if ctx.ops.process_alive(old_pid) {
                return Err(StepOutcome::failed(
                    format!(
                        "previous gateway (pid {old_pid}) is still running after a stop \
                         request; refusing to spawn a duplicate on :{}",
                        ctx.config.gateway.port
                    ),
                    log.clone(),
                ));
            }
        }
        let _ = ctx.ops.remove_file(&pid_path).await;
        Ok(())
    }

    /// Files the gateway child's stdout/stderr are captured to.
    fn gateway_stdout_log(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("logs").join("gateway.stdout.log")
    }

    fn gateway_stderr_log(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("logs").join("gateway.stderr.log")
    }

    /// Append the tail of the captured gateway logs to the step log,
    /// so a startup failure shows the process's own error output in
    /// the wizard instead of only a probe timeout.
    async fn append_gateway_log_tail(&self, ctx: &WizardContext, log: &mut Vec<String>) {
        for (label, path) in [
            ("stderr", self.gateway_stderr_log(ctx)),
            ("stdout", self.gateway_stdout_log(ctx)),
        ] {
            if let Ok(bytes) = ctx.ops.read_file(&path).await {
                let text = String::from_utf8_lossy(&bytes);
                let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                if !lines.is_empty() {
                    log.push(format!("gateway {label} ({}), last lines:", path.display()));
                    for l in lines.iter().rev().take(20).rev() {
                        log.push((*l).to_string());
                    }
                }
            }
        }
    }

    async fn wait_healthy(&self, ctx: &WizardContext, pid: u32, log: &mut Vec<String>) -> bool {
        let url = self.gateway_url(ctx);
        let headers = self.gateway_auth_headers(ctx).await;
        for attempt in 1..=self.health_retries {
            // Fail fast when the child is gone: probing a dead process
            // for the full budget reports a timeout and hides the real
            // failure, which the captured log tail then has to explain.
            if !ctx.ops.process_alive(pid) {
                log.push(format!(
                    "gateway process (pid {pid}) exited during startup (probe attempt {attempt})"
                ));
                return false;
            }
            match ctx.ops.http_get_with_headers(&url, &headers).await {
                Ok(resp) if resp.status == 200 => {
                    log.push(format!(
                        "gateway healthy on :{} (attempt {attempt})",
                        ctx.config.gateway.port
                    ));
                    return true;
                }
                Ok(resp) => log.push(format!(
                    "health probe attempt {attempt}: HTTP {}",
                    resp.status
                )),
                Err(e) => log.push(format!("health probe attempt {attempt}: {e}")),
            }
            if attempt < self.health_retries {
                tokio::time::sleep(std::time::Duration::from_millis(self.health_interval_ms)).await;
            }
        }
        false
    }
}

#[async_trait]
impl WizardStep for StartGateway {
    fn id(&self) -> &str {
        STEP_START_GATEWAY
    }
    fn name(&self) -> &str {
        "Start LiteLLM gateway"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![
            STEP_CONFIGURE_TENSOR_SPLIT.to_string(),
            STEP_DOWNLOAD_MODELS.to_string(),
        ]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        match ctx.mode {
            // The commands run() may spawn, in order (plus the uv
            // bootstrap download when uv is not yet resolvable). The two
            // venv probes always run; the venv/pip commands are skipped
            // when the probes show a current venv with litellm installed.
            RunMode::Install => {
                let (uv, needs_bootstrap) = Self::uv_argv0(ctx);
                let mut cmds = Vec::new();
                if needs_bootstrap {
                    let url = crate::uv_download_url()
                        .expect("needs_bootstrap is only true when a download URL exists");
                    let archive = Self::uv_bin_dir(ctx).join(Self::uv_archive_name());
                    cmds.push(PlannedCommand::new(vec![
                        "download".to_string(),
                        url,
                        archive.display().to_string(),
                    ]));
                }
                cmds.push(self.version_probe_command(ctx));
                cmds.push(self.litellm_probe_command(ctx));
                cmds.push(self.venv_command(ctx, &uv));
                cmds.push(self.pip_command(ctx, &uv));
                let mut spawn = self.spawn_command(ctx);
                // The gateway master key is always set at spawn (read
                // from — or first generated into — Credential
                // Manager); the plan only records that the variable
                // will be set. The value never appears here.
                spawn.env.insert(
                    "LITELLM_MASTER_KEY".to_string(),
                    "<set from credential manager>".to_string(),
                );
                let tier = &ctx.config.gateway.openrouter;
                if tier.enabled && tier.cred_ref.as_deref().is_some_and(|r| !r.is_empty()) {
                    // The real key is read from Credential Manager when
                    // the step runs; the plan only records that the
                    // variable will be set. The value never appears here.
                    spawn.env.insert(
                        "OPENROUTER_API_KEY".to_string(),
                        "<set from credential manager>".to_string(),
                    );
                }
                cmds.push(spawn);
                // Firewall reconciliation for the bind address: a
                // read-only rule query, plus the elevated script
                // when the rule state does not match the bind.
                cmds.push(self.firewall_show_command(ctx));
                cmds.push(self.firewall_elevated_command(ctx));
                cmds
            }
            // Audit only re-probes the health endpoint; nothing is spawned.
            RunMode::Audit => vec![PlannedCommand::new(vec![
                "http-get".to_string(),
                self.gateway_url(ctx),
            ])],
        }
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("re-run start-gateway to regenerate config and restart the gateway")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let mut log = Vec::new();
        match ctx.mode {
            RunMode::Install => {
                let uv = match self.ensure_uv(ctx).await {
                    Ok(p) => {
                        log.push(format!("using uv at {}", p.display()));
                        p
                    }
                    Err(outcome) => return Ok(outcome),
                };
                // Reuse a healthy existing venv: one already on the
                // pinned Python with litellm importable needs no venv
                // creation and no package install. Anything else is
                // (re)created — `uv venv --clear` replaces a venv left
                // behind by a failed install or built with an unpinned
                // (too new) Python, which previously made every retry
                // fail with "already exists" before the real error.
                let existing_version = self.venv_python_version(ctx).await;
                let venv_current =
                    existing_version.as_deref() == Some(crate::GATEWAY_PYTHON_VERSION);
                let reuse = venv_current && self.litellm_importable(ctx).await;
                if reuse {
                    log.push(format!(
                        "reusing existing venv (Python {}) — litellm already installed",
                        crate::GATEWAY_PYTHON_VERSION
                    ));
                } else if !venv_current {
                    match &existing_version {
                        Some(v) => log.push(format!(
                            "existing venv uses Python {v}; recreating it with Python {}",
                            crate::GATEWAY_PYTHON_VERSION
                        )),
                        None => log.push(format!(
                            "creating venv with Python {}",
                            crate::GATEWAY_PYTHON_VERSION
                        )),
                    }
                }
                let setup_cmds: Vec<PlannedCommand> = if reuse {
                    Vec::new()
                } else {
                    vec![self.venv_command(ctx, &uv), self.pip_command(ctx, &uv)]
                };
                for cmd in &setup_cmds {
                    log.push(format!("running: {}", cmd.argv.join(" ")));
                    match ctx.ops.run_command(cmd).await {
                        Ok(out) if out.success() => {
                            if !out.stdout.trim().is_empty() {
                                log.push(out.stdout.trim().to_string());
                            }
                        }
                        Ok(out) => {
                            return Ok(StepOutcome::failed(
                                format!(
                                    "command failed (exit {}): {}",
                                    out.exit_code,
                                    cmd.argv.join(" ")
                                ),
                                vec![out.stderr],
                            ))
                        }
                        Err(e) => {
                            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                        }
                    }
                }
                // config.yaml is generated from app state, never hand-edited.
                let yaml = match manager_gateway::generate_config_yaml(&ctx.config) {
                    Ok(y) => y,
                    Err(e) => {
                        return Ok(StepOutcome::failed(
                            format!("gateway config generation failed: {e}"),
                            vec![format!("{e:?}")],
                        ))
                    }
                };
                let cfg_path = ctx.data_dir.join("gateway").join("config.yaml");
                if let Err(e) = ctx.ops.write_file(&cfg_path, yaml.as_bytes()).await {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                }
                log.push(format!("wrote {}", cfg_path.display()));
                // Restart = stop the recorded gateway first; without
                // this the spawn below is a duplicate that fails to
                // bind while the old process keeps serving.
                if let Err(outcome) = self.stop_previous_gateway(ctx, &mut log).await {
                    return Ok(outcome);
                }
                // App-managed PostgreSQL: when install-postgres has
                // run, the server must be up before the gateway
                // spawns — the gateway receives DATABASE_URL and
                // migrates against it at startup. Not installed →
                // no DATABASE_URL, the pre-PostgreSQL behavior.
                let database_url = if postgres_installed(ctx.ops.as_ref(), &ctx.data_dir).await {
                    match ensure_postgres_running(
                        ctx.ops.as_ref(),
                        &ctx.config,
                        &ctx.data_dir,
                        &mut log,
                    )
                    .await
                    {
                        Ok(url) => {
                            // Pre-warm the Prisma toolchain: its
                            // first use downloads a Node runtime, so
                            // a bootstrap failure should surface
                            // here in the wizard, not at gateway
                            // startup. Non-fatal — the gateway start
                            // itself is the real attempt.
                            let python = self.venv_python(ctx);
                            let mut warm = PlannedCommand::new(vec![
                                python.display().to_string(),
                                "-m".to_string(),
                                "prisma".to_string(),
                                "--version".to_string(),
                            ]);
                            warm.env.insert("DATABASE_URL".to_string(), url.clone());
                            match ctx.ops.run_command(&warm).await {
                                Ok(out) if out.exit_code == 0 => {
                                    log.push("Prisma toolchain ready".to_string())
                                }
                                Ok(out) => log.push(format!(
                                    "warning: Prisma toolchain pre-warm exited {}; the gateway's first database migration may be slow or fail — see the gateway log",
                                    out.exit_code
                                )),
                                Err(e) => log.push(format!(
                                    "warning: Prisma toolchain pre-warm could not run: {e}"
                                )),
                            }
                            Some(url)
                        }
                        Err(e) => return Ok(StepOutcome::failed(e, log)),
                    }
                } else {
                    None
                };
                let (mut spawn, openrouter_warning) = match self
                    .spawn_command_with_env(ctx, database_url.as_deref())
                    .await
                {
                    Ok(pair) => pair,
                    Err(outcome) => return Ok(outcome),
                };
                if let Some(w) = &openrouter_warning {
                    log.push(format!("warning: {w}"));
                }
                // Capture the gateway's own output: without it a
                // startup failure is undiagnosable from inside the app.
                spawn.stdout_log = Some(self.gateway_stdout_log(ctx));
                spawn.stderr_log = Some(self.gateway_stderr_log(ctx));
                log.push(format!(
                    "gateway output is captured to {} and {}",
                    self.gateway_stdout_log(ctx).display(),
                    self.gateway_stderr_log(ctx).display()
                ));
                let pid = match ctx.ops.spawn_detached(&spawn).await {
                    Ok(pid) => pid,
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                };
                log.push(format!("LiteLLM spawned (pid {pid})"));
                // Record the pid so a later stop (the gateway_stop
                // command) or restart (this step re-run) terminates
                // exactly this process instead of guessing by port.
                let pid_path = crate::gateway_pid_path(&ctx.data_dir);
                match ctx
                    .ops
                    .write_file(&pid_path, pid.to_string().as_bytes())
                    .await
                {
                    Ok(()) => log.push(format!("recorded gateway pid in {}", pid_path.display())),
                    Err(e) => log.push(format!(
                        "warning: could not record the gateway pid in {}: {e}; \
                         stopping this gateway later will not be possible",
                        pid_path.display()
                    )),
                }
                if !self.wait_healthy(ctx, pid, &mut log).await {
                    self.append_gateway_log_tail(ctx, &mut log).await;
                    return Ok(StepOutcome::failed(
                        format!(
                            "gateway did not become healthy on :{} — the gateway log tail follows; full logs in {}",
                            ctx.config.gateway.port,
                            ctx.data_dir.join("logs").display()
                        ),
                        log,
                    ));
                }
                let mut message = match &openrouter_warning {
                    Some(w) => format!(
                        "LiteLLM gateway running on :{} — warning: {w}",
                        ctx.config.gateway.port
                    ),
                    None => format!("LiteLLM gateway running on :{}", ctx.config.gateway.port),
                };
                // The gateway is healthy; reconcile the Windows
                // firewall rule with the bind address so LAN clients
                // can actually reach a LAN-bound gateway.
                if let Some(fw) = self.ensure_gateway_firewall(ctx, &mut log).await {
                    log.push(format!("warning: {fw}"));
                    message = format!("{message} — warning: {fw}");
                }
                Ok(StepOutcome::done(message, log))
            }
            RunMode::Audit => {
                // Re-probe backend health without reinstalling.
                let headers = self.gateway_auth_headers(ctx).await;
                match ctx
                    .ops
                    .http_get_with_headers(&self.gateway_url(ctx), &headers)
                    .await
                {
                    Ok(resp) if resp.status == 200 => Ok(StepOutcome::done(
                        format!("gateway healthy on :{}", ctx.config.gateway.port),
                        vec![format!("GET /v1/models → 200 in {} ms", resp.elapsed_ms)],
                    )),
                    Ok(resp) => Ok(StepOutcome::failed(
                        format!("gateway unhealthy: HTTP {}", resp.status),
                        vec![],
                    )),
                    Err(e) => Ok(StepOutcome::failed(
                        format!("gateway unreachable: {e}"),
                        vec![format!("{e:?}")],
                    )),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Install VS Code extensions
// ---------------------------------------------------------------------------

/// Step 7: install Continue.dev + Cline, write the Continue config pointing
/// at the local gateway.
pub struct VscodeExtensions {
    pub extensions: Vec<String>,
}

impl Default for VscodeExtensions {
    fn default() -> Self {
        Self {
            extensions: vec![
                "Continue.continue".to_string(),
                "saoudrizwan.claude-dev".to_string(),
                // The LiteLLM provider for Copilot Chat; its endpoint
                // and API key are configured by this step (see
                // `write_litellm_endpoint`).
                "vivswan.litellm-vscode-chat".to_string(),
            ],
        }
    }
}

/// True when a command error is the ops layer's spawn-level failure
/// (the executable could not be started at all), as opposed to a
/// program that ran and exited nonzero. The ops layer reports spawn
/// failures with exit code -1 and its own `spawn failed:` prefix.
/// Steps whose external tool is optional (VS Code, the MXC runtime)
/// skip instead of failing when their executable is absent.
fn spawn_failed(e: &WizardError) -> bool {
    matches!(
        e,
        WizardError::CommandFailed { code: -1, stderr, .. }
            if stderr.starts_with("spawn failed")
    )
}

impl VscodeExtensions {
    fn install_commands(&self, cli: &str) -> Vec<PlannedCommand> {
        self.extensions
            .iter()
            .map(|ext| {
                PlannedCommand::new(vec![
                    cli.to_string(),
                    "--install-extension".to_string(),
                    ext.clone(),
                    "--force".to_string(),
                ])
            })
            .collect()
    }

    /// See [`spawn_failed`].
    fn spawn_failed(e: &WizardError) -> bool {
        spawn_failed(e)
    }

    /// The skip outcome for a machine without VS Code: the extensions
    /// are optional editor tooling, so a missing `code` executable is
    /// a skip, never a setup failure.
    fn vscode_missing() -> StepOutcome {
        StepOutcome::skipped(
            "VS Code is not installed (the `code` command was not found); extension install skipped",
        )
    }

    /// The skip outcome when a VS Code install was located but its
    /// CLI still could not be started.
    fn vscode_unusable(cli: &str) -> StepOutcome {
        StepOutcome::skipped(format!(
            "VS Code was found at {cli} but its command-line tool could not be started; extension install skipped"
        ))
    }

    fn missing_or_unusable(resolved: bool, cli: &str) -> StepOutcome {
        if resolved {
            Self::vscode_unusable(cli)
        } else {
            Self::vscode_missing()
        }
    }

    /// VS Code user settings on Windows:
    /// `%APPDATA%\Code\User\settings.json`, resolved from the home
    /// directory (`%USERPROFILE%\AppData\Roaming\...`).
    fn vscode_settings_path(home: &Path) -> PathBuf {
        home.join("AppData")
            .join("Roaming")
            .join("Code")
            .join("User")
            .join("settings.json")
    }

    fn copilot_agents_dir(home: &Path) -> PathBuf {
        home.join(".copilot").join("agents")
    }

    fn copilot_agent_path(home: &Path, group: &str) -> PathBuf {
        Self::copilot_agents_dir(home).join(format!("litellm-{group}.md"))
    }

    /// Write the LiteLLM extension's server entry (endpoint + master
    /// key) into the VS Code user settings, preserving the rest of
    /// the file (see [`merge_litellm_server_setting`]). `Ok(false)`
    /// when no home directory is known and nothing was written.
    async fn write_litellm_endpoint(
        &self,
        ctx: &WizardContext,
        log: &mut Vec<String>,
    ) -> Result<bool, StepOutcome> {
        let Some(home) = ctx.ops.home_dir() else {
            log.push("home directory unknown; LiteLLM endpoint not configured".to_string());
            return Ok(false);
        };
        let key = crate::ensure_gateway_master_key(ctx.ops.as_ref())
            .await
            .map_err(|e| {
                StepOutcome::failed(
                    format!(
                        "could not obtain the gateway master key for the LiteLLM endpoint: {e}"
                    ),
                    vec![format!("{e:?}")],
                )
            })?;
        let path = Self::vscode_settings_path(&home);
        let existing = match ctx.ops.read_file(&path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => "{}".to_string(),
        };
        let entry = litellm_server_entry(ctx.config.gateway.port, &key);
        let merged = merge_litellm_server_setting(&existing, &entry).map_err(|e| {
            StepOutcome::failed(
                format!(
                    "could not update VS Code settings at {}: {e}",
                    path.display()
                ),
                vec!["the settings file was left unchanged".to_string()],
            )
        })?;
        ctx.ops
            .write_file(&path, merged.as_bytes())
            .await
            .map_err(|e| StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))?;
        log.push(format!(
            "LiteLLM endpoint configured in {} (server \"{LITELLM_SERVER_LABEL}\", base URL {})",
            path.display(),
            entry["baseUrl"].as_str().unwrap_or_default()
        ));
        Ok(true)
    }

    /// Write one Copilot custom-agent file per gateway group into
    /// `~/.copilot/agents/`. These files are manager-owned (Pete's
    /// requirement, 2026-10-15): every run rewrites them for the
    /// currently configured groups. Returns the number written (0
    /// when no home directory is known).
    async fn write_copilot_agents(
        &self,
        ctx: &WizardContext,
        log: &mut Vec<String>,
    ) -> Result<usize, StepOutcome> {
        let Some(home) = ctx.ops.home_dir() else {
            log.push("home directory unknown; Copilot agents not written".to_string());
            return Ok(0);
        };
        let mut written = 0;
        for group in &ctx.config.gateway.groups {
            let path = Self::copilot_agent_path(&home, &group.name);
            let body = copilot_agent_markdown(&group.name);
            ctx.ops
                .write_file(&path, body.as_bytes())
                .await
                .map_err(|e| StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))?;
            log.push(format!("wrote Copilot agent {}", path.display()));
            written += 1;
        }
        Ok(written)
    }
}

#[async_trait]
impl WizardStep for VscodeExtensions {
    fn id(&self) -> &str {
        STEP_VSCODE_EXTENSIONS
    }
    fn name(&self) -> &str {
        "Install VS Code extensions"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_START_GATEWAY.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        match ctx.mode {
            // The CLI shown here is the bare command; at runtime it
            // is resolved to an absolute path when the app process's
            // PATH cannot see the install (see `run`).
            RunMode::Install => self.install_commands("code"),
            // Audit only probes which extensions are present.
            RunMode::Audit => vec![PlannedCommand::new(vec![
                "code".to_string(),
                "--list-extensions".to_string(),
            ])],
        }
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("re-run vscode-extensions to reinstall missing extensions")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let mut log = Vec::new();
        // Resolve the CLI before touching PATH: a GUI process
        // inherits the environment of whatever launched it, which
        // may predate the VS Code install (or the PATH edit its
        // installer made), so bare `code` can be unspawnable on a
        // machine where `code` works in a shell. The ops layer
        // resolves the install from the registry (App Paths) and
        // standard install locations; the bare command is the
        // fallback when nothing is found.
        let cli = ctx
            .ops
            .vscode_cli()
            .await
            .unwrap_or_else(|| "code".to_string());
        let resolved = cli != "code";
        match ctx.mode {
            RunMode::Install => {
                // Probe first: without the CLI there is nothing
                // to install into, and the step skips instead of
                // failing on the first extension.
                let probe = PlannedCommand::new(vec![cli.clone(), "--version".to_string()]);
                match ctx.ops.run_command(&probe).await {
                    Ok(_) => {}
                    Err(e) if Self::spawn_failed(&e) => {
                        return Ok(Self::missing_or_unusable(resolved, &cli))
                    }
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                }
                for cmd in self.install_commands(&cli) {
                    log.push(format!("running: {}", cmd.argv.join(" ")));
                    match ctx.ops.run_command(&cmd).await {
                        Ok(out) if out.success() => {}
                        Ok(out) => {
                            return Ok(StepOutcome::failed(
                                format!(
                                    "extension install failed (exit {}): {}",
                                    out.exit_code,
                                    cmd.argv.join(" ")
                                ),
                                vec![out.stderr],
                            ))
                        }
                        Err(e) if Self::spawn_failed(&e) => {
                            return Ok(Self::missing_or_unusable(resolved, &cli))
                        }
                        Err(e) => {
                            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                        }
                    }
                }
                // Continue: the config.yaml + .env pair (the legacy
                // config.json write is retired — see write_continue_files).
                let home = ctx.ops.home_dir();
                let continue_write = match write_continue_files(
                    ctx.ops.as_ref(),
                    &ctx.config,
                    home.as_deref(),
                    &ctx.data_dir,
                )
                .await
                {
                    Ok(w) => w,
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                };
                log.extend(continue_write.log);
                let continue_configured = continue_write.applied;
                let endpoint_configured = match self.write_litellm_endpoint(ctx, &mut log).await {
                    Ok(done) => done,
                    Err(outcome) => return Ok(outcome),
                };
                let agents_written = match self.write_copilot_agents(ctx, &mut log).await {
                    Ok(n) => n,
                    Err(outcome) => return Ok(outcome),
                };
                let mut message =
                    format!("{} VS Code extension(s) installed", self.extensions.len());
                if continue_configured {
                    message.push_str("; Continue configured");
                }
                if endpoint_configured {
                    message.push_str("; LiteLLM endpoint configured");
                }
                if agents_written > 0 {
                    message.push_str(&format!("; {agents_written} Copilot agent(s) written"));
                }
                Ok(StepOutcome::done(message, log))
            }
            RunMode::Audit => {
                let cmd = PlannedCommand::new(vec![cli.clone(), "--list-extensions".to_string()]);
                let out = match ctx.ops.run_command(&cmd).await {
                    Ok(o) => o,
                    Err(e) if Self::spawn_failed(&e) => {
                        return Ok(Self::missing_or_unusable(resolved, &cli))
                    }
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                };
                let listed = out.stdout.to_lowercase();
                let missing: Vec<&str> = self
                    .extensions
                    .iter()
                    .map(String::as_str)
                    .filter(|ext| !listed.contains(&ext.to_lowercase()))
                    .collect();
                if !missing.is_empty() {
                    return Ok(StepOutcome::failed(
                        format!("missing VS Code extensions: {}", missing.join(", ")),
                        vec![],
                    ));
                }
                // Extensions are present; also verify the pieces the
                // install run writes: the LiteLLM endpoint entry in
                // the user settings and one Copilot agent file per
                // gateway group. Only checked when a home directory
                // is known — the audit verifies what it can reach.
                let mut problems: Vec<String> = Vec::new();
                if let Some(home) = ctx.ops.home_dir() {
                    let expected_base = format!("http://127.0.0.1:{}", ctx.config.gateway.port);
                    let settings_path = Self::vscode_settings_path(&home);
                    let entry_ok = match ctx.ops.read_file(&settings_path).await {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes);
                            jsonc_parser::parse_to_serde_value(
                                &text,
                                &jsonc_parser::ParseOptions::default(),
                            )
                            .ok()
                            .flatten()
                            .and_then(|v| {
                                v.get("litellm-vscode-chat.servers")
                                    .and_then(|s| s.as_array().cloned())
                            })
                            .map(|servers| {
                                servers.iter().any(|s| {
                                    s.get("label").and_then(|v| v.as_str())
                                        == Some(LITELLM_SERVER_LABEL)
                                        && s.get("baseUrl").and_then(|v| v.as_str())
                                            == Some(expected_base.as_str())
                                        && s.get("auth")
                                            .and_then(|a| a.get("apiKey"))
                                            .and_then(|k| k.as_str())
                                            .map(|k| !k.is_empty())
                                            == Some(true)
                                })
                            })
                            .unwrap_or(false)
                        }
                        Err(_) => false,
                    };
                    if !entry_ok {
                        problems.push(
                            "LiteLLM endpoint not configured in VS Code settings".to_string(),
                        );
                    }
                    let mut missing_agents: Vec<String> = Vec::new();
                    for group in &ctx.config.gateway.groups {
                        let path = Self::copilot_agent_path(&home, &group.name);
                        if !ctx.ops.file_exists(&path).await {
                            missing_agents.push(group.name.clone());
                        }
                    }
                    if !missing_agents.is_empty() {
                        problems.push(format!(
                            "missing Copilot agent file(s): {}",
                            missing_agents.join(", ")
                        ));
                    }
                    // Continue: config.yaml must carry every gateway
                    // group with the env-referenced key, and .env
                    // must hold the key that reference resolves.
                    let cont_dir = home.join(".continue");
                    // Continue's apiBase carries /v1 (unlike the
                    // extension endpoint above, which must not).
                    let expected_api_base = format!("{expected_base}/v1");
                    let expected_key_ref = "${{ secrets.LITELLM_API_KEY }}";
                    let yaml_models: Vec<serde_yaml::Value> =
                        match ctx.ops.read_file(&cont_dir.join("config.yaml")).await {
                            Ok(bytes) => {
                                let text = String::from_utf8_lossy(&bytes).into_owned();
                                serde_yaml::from_str::<serde_yaml::Value>(&text)
                                    .ok()
                                    .and_then(|v| {
                                        v.get("models").and_then(|m| m.as_sequence().cloned())
                                    })
                                    .unwrap_or_default()
                            }
                            Err(_) => Vec::new(),
                        };
                    let missing_models: Vec<String> = ctx
                        .config
                        .gateway
                        .groups
                        .iter()
                        .filter(|g| {
                            !yaml_models.iter().any(|m| {
                                m.get("model").and_then(|v| v.as_str()) == Some(g.name.as_str())
                                    && m.get("apiBase").and_then(|v| v.as_str())
                                        == Some(expected_api_base.as_str())
                                    && m.get("apiKey").and_then(|v| v.as_str())
                                        == Some(expected_key_ref)
                            })
                        })
                        .map(|g| g.name.clone())
                        .collect();
                    if !missing_models.is_empty() {
                        problems.push(format!(
                            "Continue config.yaml missing gateway model(s): {}",
                            missing_models.join(", ")
                        ));
                    }
                    let env_ok = match ctx.ops.read_file(&cont_dir.join(".env")).await {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes);
                            text.lines().any(|line| {
                                line.trim_start()
                                    .strip_prefix("export ")
                                    .unwrap_or(line.trim_start())
                                    .strip_prefix("LITELLM_API_KEY=")
                                    .map(|v| !v.trim().is_empty())
                                    .unwrap_or(false)
                            })
                        }
                        Err(_) => false,
                    };
                    if !env_ok {
                        problems.push("Continue .env missing LITELLM_API_KEY".to_string());
                    }
                }
                if problems.is_empty() {
                    Ok(StepOutcome::done(
                        format!("{} extension(s) present", self.extensions.len()),
                        vec![],
                    ))
                } else {
                    Ok(StepOutcome::failed(
                        format!("VS Code setup incomplete: {}", problems.join("; ")),
                        vec![],
                    ))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 8. Configure MCP servers
// ---------------------------------------------------------------------------

/// Credential Manager targets. Values are never read — presence only.
pub const GITHUB_CRED_TARGET: &str = "local-llm-service-manager/github-mcp";
pub const GDRIVE_CRED_TARGET: &str = "local-llm-service-manager/gdrive-mcp";

/// Step 8: write the MCP server config (GitHub + Google Drive) with
/// credential *references*; verify the credentials exist.
pub struct McpServers;

impl McpServers {
    fn config_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("mcp").join("mcp-servers.json")
    }

    fn config_json() -> String {
        json_pretty(&serde_json::json!({
            "mcpServers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    // Credential Manager target reference — never a value.
                    "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": format!("<credential-manager:{GITHUB_CRED_TARGET}>") }
                },
                "gdrive": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-gdrive"],
                    "env": { "GOOGLE_APPLICATION_CREDENTIALS": format!("<credential-manager:{GDRIVE_CRED_TARGET}>") }
                }
            }
        }))
    }
}

#[async_trait]
impl WizardStep for McpServers {
    fn id(&self) -> &str {
        STEP_MCP_SERVERS
    }
    fn name(&self) -> &str {
        "Configure MCP servers"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_START_GATEWAY.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, _ctx: &WizardContext) -> Vec<PlannedCommand> {
        // File writes + credential presence checks; no process spawns.
        vec![]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("add the missing credential to Windows Credential Manager, then re-run mcp-servers")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let mut log = Vec::new();
        // Credential presence check first: values are never shown.
        for target in [GITHUB_CRED_TARGET, GDRIVE_CRED_TARGET] {
            match ctx.ops.credential_exists(target).await {
                Ok(true) => log.push(format!("credential '{target}' present")),
                Ok(false) => {
                    return Ok(StepOutcome::failed(
                        format!("credential '{target}' not found in Windows Credential Manager"),
                        vec![
                            "add the credential, then retry this step".to_string(),
                            "credential values are never displayed or stored".to_string(),
                        ],
                    ))
                }
                Err(e) => return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")])),
            }
        }
        let path = self.config_path(ctx);
        let json = Self::config_json();
        if let Err(e) = ctx.ops.write_file(&path, json.as_bytes()).await {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }
        log.push(format!("wrote {}", path.display()));
        Ok(StepOutcome::done(
            "MCP servers configured (github, gdrive)".to_string(),
            log,
        ))
    }
}

// ---------------------------------------------------------------------------
// 9. Register Windows ML backend
// ---------------------------------------------------------------------------

/// Step 9: probe WinMLServer, validate the model class, record the
/// registration with the researched limits verbatim.
pub struct WinMlRegister;

impl WinMlRegister {
    fn registration_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("winml").join("registration.json")
    }

    fn health_url(&self, ctx: &WizardContext) -> String {
        format!("http://127.0.0.1:{}/v1/models", ctx.config.winml.port)
    }
}

#[async_trait]
impl WizardStep for WinMlRegister {
    fn id(&self) -> &str {
        STEP_WINML_REGISTER
    }
    fn name(&self) -> &str {
        "Register Windows ML backend"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_START_GATEWAY.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        // The probe is an HTTP GET, surfaced here for transparency.
        vec![PlannedCommand::new(vec![
            "http-get".to_string(),
            self.health_url(ctx),
        ])]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("start WinMLServer, then re-run winml-register")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        if !ctx.config.winml.enabled {
            return Ok(StepOutcome::skipped(
                "Windows ML backend disabled in config",
            ));
        }
        let model_id = match &ctx.config.winml.model_id {
            Some(m) => m.clone(),
            None => {
                return Ok(StepOutcome::failed(
                    "winml.model_id not configured",
                    vec!["set winml.model_id to a tool-runner-class model".to_string()],
                ))
            }
        };
        // Authoritative eligibility check lives in manager-winml.
        if let Err(e) = manager_winml::model_class_eligible(&model_id) {
            return Ok(StepOutcome::failed(
                format!("model '{model_id}' ineligible for Windows ML: {e}"),
                vec![format!("{e:?}")],
            ));
        }
        let mut log = vec![format!("model '{model_id}' eligible (tool-runner class)")];
        match ctx.ops.http_get(&self.health_url(ctx)).await {
            Ok(resp) if resp.status == 200 => {
                log.push(format!(
                    "WinMLServer responding on :{}",
                    ctx.config.winml.port
                ));
            }
            Ok(resp) => {
                return Ok(StepOutcome::failed(
                    format!("WinMLServer unhealthy: HTTP {}", resp.status),
                    log,
                ))
            }
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!(
                        "WinMLServer not reachable on :{}: {e}",
                        ctx.config.winml.port
                    ),
                    log,
                ))
            }
        }
        // Record the registration with the researched limits verbatim.
        let limits = manager_winml::WinMlLimits::known_limits();
        let reg = serde_json::json!({
            "model_id": model_id,
            "port": ctx.config.winml.port,
            "endpoint": self.health_url(ctx),
            "limits": limits,
            "registered_at": chrono::Utc::now().to_rfc3339(),
        });
        let path = self.registration_path(ctx);
        if let Err(e) = ctx
            .ops
            .write_file(&path, json_pretty(&reg).as_bytes())
            .await
        {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }
        log.push(format!("wrote {}", path.display()));
        for stmt in manager_winml::WinMlLimits::statements() {
            log.push(format!("limit: {stmt}"));
        }
        Ok(StepOutcome::done(
            format!(
                "Windows ML backend registered: {model_id} on :{} (experimental, no tensor-split)",
                ctx.config.winml.port
            ),
            log,
        ))
    }
}

// ---------------------------------------------------------------------------
// 10. Install MXC runtime
// ---------------------------------------------------------------------------

/// Verified pin for the Microsoft MXC SDK distribution the wizard
/// installs: `@microsoft/mxc-sdk` 1.0.0 (published 2026-10-06). The
/// package has no install scripts and no PATH shims — the runtime is
/// payload the SDK locates relative to its own package root — so the
/// step downloads the npm tarball itself, verifies its SHA-256,
/// extracts only `package/bin/x64/` (the whole folder: wxc-exec.exe
/// resolves sibling daemons and DLLs from its own directory at
/// runtime) into `<data_dir>/mxc/bin/x64/`, and verifies both named
/// executables against their pins. All hashes were computed from the
/// actual registry tarball on 2026-10-15 (tarball SHA-256
/// 39e59ffd…; registry SHA-1 5e211c18… cross-checked).
#[derive(Debug, Clone)]
pub struct MxcSdkPin {
    pub version: String,
    pub tarball_asset: String,
    pub tarball_sha256: String,
    pub tarball_size: u64,
    pub wxc_exec_sha256: String,
    pub wxc_host_prep_sha256: String,
}

impl MxcSdkPin {
    pub const TARBALL_BASE: &'static str = "https://registry.npmjs.org/@microsoft/mxc-sdk/-";

    pub fn default_pin() -> Self {
        Self {
            version: "1.0.0".to_string(),
            tarball_asset: "mxc-sdk-1.0.0.tgz".to_string(),
            tarball_sha256: "39e59ffd9d0832334889b0cd583d0beb43b1bbbe9f783dd7c6980203d23e9602"
                .to_string(),
            tarball_size: 37_746_375,
            wxc_exec_sha256: "8a6d1db4dd1846981127d9014741be18b094d07be967765dff6754bda1d40259"
                .to_string(),
            wxc_host_prep_sha256:
                "c07c94acf62844c5c20590a2d3e7944d0beaaf424df934afc8c83d531bf72825".to_string(),
        }
    }

    pub fn tarball_url(&self) -> String {
        format!("{}/{}", Self::TARBALL_BASE, self.tarball_asset)
    }
}

/// The app-installed MXC runtime directory: `<data_dir>/mxc/bin/x64`,
/// mirroring the SDK package layout (`bin/<arch>/`).
fn mxc_bin_dir(ctx: &WizardContext) -> PathBuf {
    ctx.data_dir.join("mxc").join("bin").join("x64")
}

/// Absolute path of the app-installed `wxc-exec.exe`.
pub fn mxc_wxc_exec_path(ctx: &WizardContext) -> PathBuf {
    mxc_bin_dir(ctx).join("wxc-exec.exe")
}

fn mxc_host_prep_path(ctx: &WizardContext) -> PathBuf {
    mxc_bin_dir(ctx).join("wxc-host-prep.exe")
}

/// Resolve the wxc-exec invocation for the MXC steps: the
/// app-installed runtime when install-mxc has placed it, else the
/// bare program name (a system install on PATH).
async fn resolve_wxc_exec(ctx: &WizardContext) -> String {
    let installed = mxc_wxc_exec_path(ctx);
    if ctx.ops.file_exists(&installed).await {
        installed.display().to_string()
    } else {
        "wxc-exec.exe".to_string()
    }
}

/// Step 10: install the MXC runtime (pinned `@microsoft/mxc-sdk`
/// binaries) under the app data dir and run the one-time elevated
/// host preparation. MXC is installed for setup completeness — Pete's
/// decision (2026-10-15): nothing in the app routes MCP launches
/// through wxc-exec yet, so the policy step that follows records
/// configuration only.
pub struct InstallMxc {
    pub pin: Option<MxcSdkPin>,
}

impl InstallMxc {
    pub fn unconfigured() -> Self {
        Self { pin: None }
    }
    pub fn with_pin(pin: MxcSdkPin) -> Self {
        Self { pin: Some(pin) }
    }

    fn install_root(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("mxc")
    }

    fn marker_path(&self, ctx: &WizardContext) -> PathBuf {
        self.install_root(ctx).join(".installed.json")
    }

    fn tarball_path(&self, ctx: &WizardContext, pin: &MxcSdkPin) -> PathBuf {
        self.install_root(ctx)
            .join("downloads")
            .join(&pin.tarball_asset)
    }

    fn prep_script_path(&self, ctx: &WizardContext) -> PathBuf {
        self.install_root(ctx).join("host-prep.cmd")
    }

    /// The elevated launch that performs host preparation: a small
    /// script chaining both wxc-host-prep subcommands, so the user
    /// sees exactly one UAC prompt. `prepare-system-drive` persists
    /// across reboots; `prepare-null-device` reapplies the
    /// `\Device\Null` security descriptor, which the kernel resets
    /// every boot — it is re-run here so a completed setup works
    /// immediately, and will need re-applying before sandboxed runs
    /// after a reboot (owned by the future enforcement phase, not
    /// this setup step).
    fn elevated_prep_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            self.prep_script_path(ctx).display().to_string(),
        ])
    }

    fn prep_script_contents(&self, ctx: &WizardContext) -> String {
        let prep = mxc_host_prep_path(ctx).display().to_string();
        format!(
            "@echo off\r\n\"{prep}\" prepare-system-drive --quiet\r\n\"{prep}\" prepare-null-device --quiet\r\n"
        )
    }

    async fn binaries_verified(&self, ctx: &WizardContext, pin: &MxcSdkPin) -> bool {
        let exec_ok = matches!(
            ctx.ops.sha256_file(&mxc_wxc_exec_path(ctx)).await,
            Ok(h) if checksums_match(&h, &pin.wxc_exec_sha256)
        );
        let prep_ok = matches!(
            ctx.ops.sha256_file(&mxc_host_prep_path(ctx)).await,
            Ok(h) if checksums_match(&h, &pin.wxc_host_prep_sha256)
        );
        exec_ok && prep_ok
    }

    async fn read_marker(&self, ctx: &WizardContext) -> Option<serde_json::Value> {
        let raw = ctx.ops.read_file(&self.marker_path(ctx)).await.ok()?;
        serde_json::from_slice(&raw).ok()
    }

    fn marker_prep_done(marker: &Option<serde_json::Value>) -> bool {
        marker
            .as_ref()
            .and_then(|m| m.get("host_prep"))
            .map(|p| {
                p.get("system_drive").and_then(|v| v.as_bool()) == Some(true)
                    && p.get("null_device").and_then(|v| v.as_bool()) == Some(true)
            })
            .unwrap_or(false)
    }

    async fn write_marker(
        &self,
        ctx: &WizardContext,
        pin: &MxcSdkPin,
        prep_done: bool,
    ) -> Result<(), WizardError> {
        let marker = serde_json::json!({
            "version": pin.version,
            "tarball_sha256": pin.tarball_sha256,
            "wxc_exec_sha256": pin.wxc_exec_sha256,
            "wxc_host_prep_sha256": pin.wxc_host_prep_sha256,
            "host_prep": {
                "system_drive": prep_done,
                "null_device": prep_done,
                "at": chrono::Utc::now().to_rfc3339(),
            },
        });
        ctx.ops
            .write_file(&self.marker_path(ctx), json_pretty(&marker).as_bytes())
            .await
    }
}

#[async_trait]
impl WizardStep for InstallMxc {
    fn id(&self) -> &str {
        STEP_INSTALL_MXC
    }
    fn name(&self) -> &str {
        "Install MXC runtime"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_DETECT_HARDWARE.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        let Some(pin) = self.pin.as_ref() else {
            return Vec::new();
        };
        vec![
            PlannedCommand::new(vec![
                "download".to_string(),
                pin.tarball_url(),
                self.tarball_path(ctx, pin).display().to_string(),
            ]),
            self.elevated_prep_command(ctx),
        ]
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("re-run install-mxc and accept the Windows elevation prompt so wxc-host-prep can prepare the host")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let pin = match &self.pin {
            Some(p) => p.clone(),
            None => {
                return Ok(StepOutcome::failed(
                    "no MXC SDK build pinned",
                    vec![
                        "this build ships without a built-in MXC SDK pin; that is a packaging error in the app, not a configuration problem".to_string(),
                        "failing closed: will not download an unpinned runtime".to_string(),
                    ],
                ))
            }
        };
        let root = self.install_root(ctx);
        let mut log = vec![format!(
            "pinned MXC SDK: @microsoft/mxc-sdk@{} ({})",
            pin.version, pin.tarball_asset
        )];

        match ctx.mode {
            RunMode::Audit => {
                if !ctx.ops.file_exists(&self.marker_path(ctx)).await {
                    return Ok(StepOutcome::failed(
                        "MXC runtime not installed; run the install-mxc step",
                        vec![format!("{} not found", self.marker_path(ctx).display())],
                    ));
                }
                if !self.binaries_verified(ctx, &pin).await {
                    return Ok(StepOutcome::failed(
                        format!(
                            "installed MXC binaries fail checksum verification against the pinned SDK {}",
                            pin.version
                        ),
                        log,
                    ));
                }
                // Host preparation is deliberately not audited: the
                // null-device preparation resets every boot by design,
                // so an audit after any reboot would always fail.
                return Ok(StepOutcome::done(
                    format!("MXC runtime {} verified", pin.version),
                    log,
                ));
            }
            RunMode::Install => {}
        }

        // Install the binaries unless the pinned pair already verifies.
        if self.binaries_verified(ctx, &pin).await {
            log.push("MXC binaries already installed with matching checksums".to_string());
        } else {
            let tarball = self.tarball_path(ctx, &pin);
            let tarball_ok = ctx.ops.file_exists(&tarball).await
                && matches!(ctx.ops.sha256_file(&tarball).await, Ok(h) if checksums_match(&h, &pin.tarball_sha256));
            if !tarball_ok {
                let url = pin.tarball_url();
                log.push(format!("downloading {url}"));
                match ctx.ops.download(&url, &tarball, &|_, _| {}).await {
                    Ok(bytes) => log.push(format!("downloaded {bytes} bytes")),
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                }
                let actual = match ctx.ops.sha256_file(&tarball).await {
                    Ok(h) => h,
                    Err(e) => {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]))
                    }
                };
                if !checksums_match(&actual, &pin.tarball_sha256) {
                    return Ok(StepOutcome::failed(
                        format!(
                            "checksum mismatch for {}: expected {}, got {}",
                            tarball.display(),
                            pin.tarball_sha256,
                            actual
                        ),
                        vec!["tarball rejected; not installed".to_string()],
                    ));
                }
                log.push(format!("SHA-256 verified for {}", pin.tarball_asset));
            } else {
                log.push(format!(
                    "{} already present with matching checksum",
                    pin.tarball_asset
                ));
            }
            if let Err(e) = ctx
                .ops
                .extract_tar_gz(&tarball, &root, "package/bin/x64/", "package/")
                .await
            {
                return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
            }
            log.push(format!(
                "extracted package/bin/x64 to {}",
                mxc_bin_dir(ctx).display()
            ));
            if !self.binaries_verified(ctx, &pin).await {
                return Ok(StepOutcome::failed(
                    "extraction did not produce the pinned wxc executables",
                    vec![
                        format!("expected {} and {}", mxc_wxc_exec_path(ctx).display(), mxc_host_prep_path(ctx).display()),
                        "the tarball verified against its pin, so its layout differs from the pinned SDK's — failing closed".to_string(),
                    ],
                ));
            }
        }

        // One-time elevated host preparation. Skipped when a previous
        // run already recorded it — re-prompting UAC on every wizard
        // run would be hostile, and prepare-system-drive persists.
        let marker = self.read_marker(ctx).await;
        if Self::marker_prep_done(&marker) {
            log.push("host preparation already recorded; elevation not requested".to_string());
            return Ok(StepOutcome::done(
                format!(
                    "MXC runtime {} installed to {}; host preparation complete",
                    pin.version,
                    mxc_bin_dir(ctx).display()
                ),
                log,
            ));
        }
        if let Err(e) = ctx
            .ops
            .write_file(
                &self.prep_script_path(ctx),
                self.prep_script_contents(ctx).as_bytes(),
            )
            .await
        {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }
        let prep_note = match ctx.ops.run_elevated(&self.elevated_prep_command(ctx)).await {
            Ok(out) if out.success() => {
                log.push(
                    "host preparation completed (prepare-system-drive, prepare-null-device)"
                        .to_string(),
                );
                None
            }
            Ok(out) => Some(format!(
                "host preparation exited with code {}",
                out.exit_code
            )),
            Err(e) => Some(format!("host preparation was not completed: {e}")),
        };
        match prep_note {
            None => {
                if let Err(e) = self.write_marker(ctx, &pin, true).await {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                }
                Ok(StepOutcome::done(
                    format!(
                        "MXC runtime {} installed to {}; host preparation complete",
                        pin.version,
                        mxc_bin_dir(ctx).display()
                    ),
                    log,
                ))
            }
            Some(note) => {
                // The runtime itself is installed and verified; only
                // the elevated preparation is outstanding. Record it
                // as not-done so a re-run retries the elevation
                // without re-downloading, and say so in the message —
                // the AppContainer tier stays unavailable until then.
                if let Err(e) = self.write_marker(ctx, &pin, false).await {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                }
                log.push(note.clone());
                Ok(StepOutcome::done(
                    format!(
                        "MXC runtime {} installed to {}; {note} — re-run this step and accept the elevation prompt to finish host preparation",
                        pin.version,
                        mxc_bin_dir(ctx).display()
                    ),
                    log,
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 11. Apply MXC policies
// ---------------------------------------------------------------------------

/// Step 11: probe MXC, write the default policy JSON, validate it,
/// run a harmless sandboxed self-test.
pub struct MxcPolicies;

impl MxcPolicies {
    fn policy_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("mxc").join("policy.json")
    }

    /// The probe is `wxc-exec --probe`: "run the fallback detector
    /// and emit JSON, without spawning a sandbox" — the SDK's own
    /// invocation, verified against the pinned 1.0.0 binary. An
    /// earlier `--probe-json` form does not exist in that binary.
    fn probe_command(exe: &str) -> PlannedCommand {
        PlannedCommand::new(vec![exe.to_string(), "--probe".to_string()])
    }

    /// A self-test is the pinned policy loaded in `--dry-run` mode
    /// (config parse + tier selection, nothing spawned) with a
    /// harmless command attached, per the 1.0.0 CLI contract:
    /// `wxc-exec [--config <policy>] [--dry-run] [-- <command>]`. The
    /// earlier `--policy ... --self-test` form does not exist in the
    /// real binary.
    fn self_test_command(&self, ctx: &WizardContext, exe: &str) -> PlannedCommand {
        PlannedCommand::new(vec![
            exe.to_string(),
            "--config".to_string(),
            self.policy_path(ctx).display().to_string(),
            "--dry-run".to_string(),
            "--".to_string(),
            "cmd.exe".to_string(),
            "/c".to_string(),
            "echo llm-manager-mxc-self-test".to_string(),
        ])
    }
}

#[async_trait]
impl WizardStep for MxcPolicies {
    fn id(&self) -> &str {
        STEP_MXC_POLICIES
    }
    fn name(&self) -> &str {
        "Apply MXC policies"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_DETECT_HARDWARE.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        // Planned commands name the canonical app-installed runtime;
        // run() resolves the same path when it exists and falls back
        // to a PATH install otherwise.
        let exe = mxc_wxc_exec_path(ctx).display().to_string();
        match ctx.mode {
            RunMode::Install => {
                vec![Self::probe_command(&exe), self.self_test_command(ctx, &exe)]
            }
            RunMode::Audit => vec![Self::probe_command(&exe)],
        }
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("verify the MXC runtime is installed (run install-mxc), then re-run mxc-policies")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let mut log = Vec::new();
        let exe = resolve_wxc_exec(ctx).await;
        // Probe PSEC/ingress availability.
        match ctx.ops.run_command(&Self::probe_command(&exe)).await {
            Ok(out) if out.success() => {
                log.push("MXC probe succeeded".to_string());
                let stdout = out.stdout.trim();
                if !stdout.is_empty() {
                    log.push(format!(
                        "probe: {}",
                        stdout.chars().take(300).collect::<String>()
                    ));
                }
            }
            Ok(out) => {
                return Ok(StepOutcome::failed(
                    format!("MXC probe failed (exit {})", out.exit_code),
                    vec![out.stderr],
                ))
            }
            Err(e) if spawn_failed(&e) => {
                // The MXC runtime is a separate Microsoft distribution
                // (@microsoft/mxc-sdk); it is not installed by this
                // app. Absent runtime = optional component missing,
                // a skip — the same treatment as a missing VS Code.
                return Ok(StepOutcome::skipped(
                    "MXC runtime (wxc-exec.exe) is not installed; MXC policy application skipped",
                ));
            }
            Err(e) => {
                return Ok(StepOutcome::failed(
                    format!("MXC runtime not available: {e}"),
                    vec![format!("{e:?}")],
                ))
            }
        }
        let mode = ctx.config.mxc.mode;
        let path = self.policy_path(ctx);
        match ctx.mode {
            RunMode::Install => {
                let policy = manager_mxc::Policy::default_policy();
                let report = manager_mxc::validate(&policy)
                    .map_err(|e| WizardError::Mxc(format!("policy validation error: {e}")))?;
                if !report.valid {
                    return Ok(StepOutcome::failed(
                        "default MXC policy failed validation".to_string(),
                        report.errors,
                    ));
                }
                for w in &report.warnings {
                    log.push(format!("policy warning: {w}"));
                }
                let json = serde_json::to_string_pretty(&policy)
                    .map_err(|e| WizardError::Mxc(format!("policy serialization failed: {e}")))?;
                if let Err(e) = ctx.ops.write_file(&path, json.as_bytes()).await {
                    return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                }
                log.push(format!("wrote default policy to {}", path.display()));
                log.push(format!("mode: {mode:?} (Learning recommended first)"));
                // Harmless sandboxed self-test; activity shown inline.
                match ctx
                    .ops
                    .run_command(&self.self_test_command(ctx, &exe))
                    .await
                {
                    Ok(out) if out.success() => {
                        log.push("policy self-test passed".to_string());
                        if !out.stdout.trim().is_empty() {
                            log.push(format!(
                                "self-test: {}",
                                out.stdout.trim().chars().take(300).collect::<String>()
                            ));
                        }
                    }
                    Ok(out) => {
                        return Ok(StepOutcome::failed(
                            format!("MXC self-test failed (exit {})", out.exit_code),
                            vec![out.stderr],
                        ))
                    }
                    Err(e) => {
                        return Ok(StepOutcome::failed(
                            format!("MXC self-test error: {e}"),
                            vec![format!("{e:?}")],
                        ))
                    }
                }
                Ok(StepOutcome::done(
                    format!("MXC policy applied ({mode:?} mode)"),
                    log,
                ))
            }
            RunMode::Audit => {
                // Re-probe + re-validate the on-disk policy; no reinstall.
                let data = match ctx.ops.read_file(&path).await {
                    Ok(d) => d,
                    Err(_) => {
                        return Ok(StepOutcome::failed(
                            "MXC policy.json missing; re-run mxc-policies".to_string(),
                            vec![format!("{} not found", path.display())],
                        ))
                    }
                };
                let policy: manager_mxc::Policy = match serde_json::from_slice(&data) {
                    Ok(p) => p,
                    Err(e) => {
                        return Ok(StepOutcome::failed(
                            format!("policy.json unparseable: {e}"),
                            vec![],
                        ))
                    }
                };
                let report = manager_mxc::validate(&policy)
                    .map_err(|e| WizardError::Mxc(format!("policy validation error: {e}")))?;
                if !report.valid {
                    return Ok(StepOutcome::failed(
                        "installed MXC policy failed validation".to_string(),
                        report.errors,
                    ));
                }
                Ok(StepOutcome::done(
                    format!("MXC policy valid ({mode:?} mode)"),
                    vec![format!("validated {}", path.display())],
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 12. Smoke-test inference
// ---------------------------------------------------------------------------

/// Sanity bound for a smoke-test request (non-streaming proxy for TTFT).
pub const SMOKE_MAX_LATENCY_MS: u64 = 120_000;

/// Step 12: tagged test request per gateway group through the gateway;
/// assert non-empty completion + sane latency; record baselines.
pub struct SmokeTest {
    pub max_latency_ms: u64,
    /// Install mode only: how long to wait for each enabled member
    /// backend to answer its health endpoint before probing the
    /// gateway (llama-server needs time to load its model; the wizard
    /// starts the backends right before this step).
    pub ready_budget_ms: u64,
    /// Delay between health polls while waiting.
    pub ready_poll_ms: u64,
}

impl Default for SmokeTest {
    fn default() -> Self {
        Self {
            max_latency_ms: SMOKE_MAX_LATENCY_MS,
            ready_budget_ms: 240_000,
            ready_poll_ms: 2_000,
        }
    }
}

impl SmokeTest {
    fn baselines_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("wizard").join("smoke-baselines.json")
    }

    /// Wait until every enabled backend a gateway group routes to
    /// answers `GET /health` with 200 (llama-server returns 503 while
    /// its model loads and refuses connections before it binds).
    /// Returns Some(failed outcome) when a backend does not become
    /// healthy inside the budget; None when probing may proceed.
    /// The probe request body. The probe verifies routing and
    /// generation, not reasoning: thinking models (the planner)
    /// otherwise spend the whole token budget inside the think
    /// block and return an empty visible completion (found on real
    /// hardware 2026-10-15 — with max_tokens at 8, all 8 generated
    /// tokens were reasoning and the step reported "empty
    /// completion" against a healthy backend). Thinking is disabled
    /// for the probe; templates that do not know the flag ignore it.
    fn probe_body(group: &str, tag: &str) -> String {
        serde_json::json!({
            "model": group,
            "messages": [{ "role": "user", "content": format!("wizard smoke test {tag}") }],
            "max_tokens": 128,
            "stream": false,
            "chat_template_kwargs": { "enable_thinking": false },
        })
        .to_string()
    }

    async fn wait_for_backends(
        &self,
        ctx: &WizardContext,
        log: &mut Vec<String>,
    ) -> Option<StepOutcome> {
        let mut targets: Vec<(String, u16)> = Vec::new();
        for group in &ctx.config.gateway.groups {
            for member in &group.members {
                if let Some(b) = ctx.config.backends.iter().find(|b| &b.id == member) {
                    if b.enabled && !targets.iter().any(|(id, _)| id == &b.id) {
                        targets.push((b.id.clone(), b.port));
                    }
                }
            }
        }
        for (id, port) in targets {
            let url = format!("http://127.0.0.1:{port}/health");
            let start = std::time::Instant::now();
            loop {
                if let Ok(resp) = ctx.ops.http_get(&url).await {
                    if resp.status == 200 {
                        log.push(format!("backend '{id}' healthy on :{port}"));
                        break;
                    }
                }
                if start.elapsed().as_millis() as u64 >= self.ready_budget_ms {
                    return Some(StepOutcome::failed(
                        format!(
                            "backend '{id}' did not become healthy within {} s; start it from the Backends view, then re-run smoke-test",
                            self.ready_budget_ms / 1000
                        ),
                        log.clone(),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(self.ready_poll_ms.max(1)))
                    .await;
            }
        }
        None
    }
}

#[async_trait]
impl WizardStep for SmokeTest {
    fn id(&self) -> &str {
        STEP_SMOKE_TEST
    }
    fn name(&self) -> &str {
        "Smoke-test inference"
    }
    fn prerequisites(&self) -> Vec<String> {
        vec![STEP_START_GATEWAY.to_string()]
    }
    fn is_check(&self) -> bool {
        true
    }
    fn planned_commands(&self, ctx: &WizardContext) -> Vec<PlannedCommand> {
        // The probes are HTTP POSTs, surfaced here for transparency.
        ctx.config
            .gateway
            .groups
            .iter()
            .map(|g| {
                PlannedCommand::new(vec![
                    "http-post".to_string(),
                    format!(
                        "http://{}:{}/v1/chat/completions",
                        gateway_probe_host(&ctx.config),
                        ctx.config.gateway.port
                    ),
                    Self::probe_body(&g.name, "wizard-smoke"),
                ])
            })
            .collect()
    }
    fn remediation_hint(&self) -> Option<&str> {
        Some("check backend health on the dashboard, then re-run smoke-test")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let groups: Vec<String> = ctx
            .config
            .gateway
            .groups
            .iter()
            .map(|g| g.name.clone())
            .collect();
        if groups.is_empty() {
            return Ok(StepOutcome::failed(
                "no gateway model groups configured",
                vec![],
            ));
        }
        let mut log = Vec::new();
        if ctx.mode == RunMode::Install {
            if let Some(outcome) = self.wait_for_backends(ctx, &mut log).await {
                return Ok(outcome);
            }
        }
        let tag = format!("wizard-{}", chrono::Utc::now().timestamp());
        let url = format!(
            "http://{}:{}/v1/chat/completions",
            gateway_probe_host(&ctx.config),
            ctx.config.gateway.port
        );
        // The gateway requires its master key when one is set; it is
        // read from Credential Manager, never generated here.
        let headers: Vec<(String, String)> = match ctx
            .ops
            .credential_read(crate::LITELLM_MASTER_KEY_CRED_TARGET)
            .await
        {
            Some(key) if !key.is_empty() => {
                vec![("Authorization".to_string(), format!("Bearer {key}"))]
            }
            _ => Vec::new(),
        };
        let mut baselines = Vec::new();
        for group in &groups {
            let body = Self::probe_body(group, &tag);
            let resp = match ctx.ops.http_post_with_headers(&url, &body, &headers).await {
                Ok(r) => r,
                Err(e) => {
                    return Ok(StepOutcome::failed(
                        format!("smoke test for '{group}' failed: {e}"),
                        vec![format!("{e:?}")],
                    ))
                }
            };
            if resp.status != 200 {
                return Ok(StepOutcome::failed(
                    format!("smoke test for '{group}': HTTP {}", resp.status),
                    vec![resp.body_str()],
                ));
            }
            let v: serde_json::Value = match serde_json::from_slice(&resp.body) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(StepOutcome::failed(
                        format!("unparseable completion for '{group}': {e}"),
                        log,
                    ))
                }
            };
            let mut content = v
                .pointer("/choices/0/message/content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            // A response that is entirely reasoning still proves
            // routing and generation (the server splits reasoning
            // into its own field, and a template that ignored
            // enable_thinking=false leaves the visible completion
            // empty within the token budget).
            let mut reasoning_only = false;
            if content.is_empty() {
                let reasoning = v
                    .pointer("/choices/0/message/reasoning_content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !reasoning.is_empty() {
                    content = reasoning;
                    reasoning_only = true;
                }
            }
            if content.is_empty() {
                let finish = v
                    .pointer("/choices/0/finish_reason")
                    .and_then(|c| c.as_str())
                    .unwrap_or("unknown");
                return Ok(StepOutcome::failed(
                    format!("smoke test for '{group}': empty completion (finish_reason: {finish})"),
                    log,
                ));
            }
            if resp.elapsed_ms > self.max_latency_ms {
                return Ok(StepOutcome::failed(
                    format!(
                        "smoke test for '{group}': latency {} ms exceeded sanity bound {} ms",
                        resp.elapsed_ms, self.max_latency_ms
                    ),
                    log,
                ));
            }
            if reasoning_only {
                log.push(format!(
                    "{group}: {} ms, reasoning-only response ({} chars)",
                    resp.elapsed_ms,
                    content.chars().count()
                ));
            } else {
                log.push(format!(
                    "{group}: {} ms, {} chars",
                    resp.elapsed_ms,
                    content.chars().count()
                ));
            }
            baselines.push(serde_json::json!({
                "group": group,
                "latency_ms": resp.elapsed_ms,
                "chars": content.chars().count(),
            }));
        }
        let doc = serde_json::json!({
            "tag": tag,
            "baselines": baselines,
            "recorded_at": chrono::Utc::now().to_rfc3339(),
        });
        let path = self.baselines_path(ctx);
        if let Err(e) = ctx
            .ops
            .write_file(&path, json_pretty(&doc).as_bytes())
            .await
        {
            return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
        }
        log.push(format!("baselines written to {}", path.display()));
        Ok(StepOutcome::done(
            format!("smoke test passed for {} group(s)", groups.len()),
            log,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeSystemOps;
    use crate::{GpuDescriptor, RunMode, StepState, WizardContext};
    use manager_config::{AppConfig, ComponentUpdate, ModelConfig};
    use sha2::{Digest, Sha256};
    use std::sync::Arc;

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    fn test_gpus() -> Vec<GpuDescriptor> {
        vec![
            GpuDescriptor {
                index: 0,
                name: "NVIDIA RTX A4000".to_string(),
                uuid: "GPU-aaaa".to_string(),
                total_vram_mib: 16384,
                compute_capability: (8, 6),
                driver_version: "581.57".to_string(),
            },
            GpuDescriptor {
                index: 1,
                name: "Tesla P100-PCIE-16GB".to_string(),
                uuid: "GPU-bbbb".to_string(),
                total_vram_mib: 16384,
                compute_capability: (6, 0),
                driver_version: "581.57".to_string(),
            },
            GpuDescriptor {
                index: 2,
                name: "Tesla P100-PCIE-16GB".to_string(),
                uuid: "GPU-cccc".to_string(),
                total_vram_mib: 16384,
                compute_capability: (6, 0),
                driver_version: "581.57".to_string(),
            },
        ]
    }

    fn test_pin() -> LlamaCppPin {
        LlamaCppPin {
            tag: "b9999".to_string(),
            asset: "llama-b9999-bin-win-cuda-12.4-x64.zip".to_string(),
            sha256: sha256_hex(b"fake-zip-bytes"),
            runtime: LlamaCppAsset {
                asset: "cudart-llama-bin-win-cuda-12.4-x64.zip".to_string(),
                sha256: sha256_hex(b"fake-cudart-bytes"),
            },
        }
    }

    /// Script both archive downloads for `pin` under their real URLs.
    fn script_pin_downloads(ops: &FakeSystemOps, pin: &LlamaCppPin) {
        ops.with_download(&pin.download_url(), b"fake-zip-bytes".to_vec());
        ops.with_download(&pin.runtime_download_url(), b"fake-cudart-bytes".to_vec());
    }

    fn test_config_with_models() -> AppConfig {
        let mut c = AppConfig::default_config();
        for b in c.backends.iter_mut() {
            b.enabled = true;
        }
        c.models = vec![
            ModelConfig {
                id: "qwen3-8b".to_string(),
                gguf_path: "models/qwen3-8b.gguf".into(),
                quant: "Q4_K_M".to_string(),
                params_b: 8.0,
                sha256: sha256_hex(b"gguf-8b-bytes"),
                verified_at: None,
                source_url: "https://example.com/qwen3-8b.gguf".to_string(),
                assigned_backends: vec!["tool-runner".to_string()],
                enabled: true,
            },
            ModelConfig {
                id: "qwen2.5-coder-7b".to_string(),
                gguf_path: "models/qwen2.5-coder-7b.gguf".into(),
                quant: "Q4_K_M".to_string(),
                params_b: 7.0,
                sha256: sha256_hex(b"gguf-7b-bytes"),
                verified_at: None,
                source_url: "https://example.com/qwen2.5-coder-7b.gguf".to_string(),
                assigned_backends: vec!["coder-fast".to_string()],
                enabled: true,
            },
        ];
        c
    }

    fn ctx_with(ops: Arc<FakeSystemOps>, mode: RunMode) -> WizardContext {
        WizardContext::new(test_config_with_models(), PathBuf::from("/data"), mode, ops)
    }

    /// Serializes every test that swaps the process-global PATH.
    /// PATH is shared by all test threads in this process, so two
    /// PathGuard tests running in parallel can restore each other's
    /// PATH mid-test (the intermittent start-gateway failures: one
    /// test resolves uv against another test's PATH). PathGuard
    /// holds this lock from construction until drop, so only one
    /// swapped PATH is ever live at a time.
    static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Guard that swaps PATH for the duration of a test. uv resolution
    /// reads the real process PATH, so tests that assert on it must
    /// control it. Restores on drop. Holds [`PATH_LOCK`] for its whole
    /// lifetime so PATH-swapping tests never overlap.
    struct PathGuard {
        old: Option<std::ffi::OsString>,
        // Held purely for its Drop, which releases PATH_LOCK.
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl PathGuard {
        fn set_to(dir: &std::path::Path) -> Self {
            // Recover from poisoning: a panicking holder must not
            // wedge every later PATH test in the process.
            let lock = PATH_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let old = std::env::var_os("PATH");
            std::env::set_var("PATH", dir.as_os_str());
            Self { old, _lock: lock }
        }
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            match self.old.take() {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    /// Temp dir acting as a PATH entry. When `with_uv` is true it contains
    /// a fake `uv` binary so uv resolution hits PATH deterministically.
    fn path_dir_with_uv(with_uv: bool) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-wiz-uv-{}-{}",
            std::process::id(),
            if with_uv { "hit" } else { "miss" }
        ));
        std::fs::create_dir_all(&dir).unwrap();
        if with_uv {
            let name = if cfg!(target_os = "windows") {
                "uv.exe"
            } else {
                "uv"
            };
            std::fs::write(dir.join(name), b"fake-uv").unwrap();
        }
        dir
    }

    fn smoke_post_response() -> crate::HttpResponse {
        FakeSystemOps::http_ok(r#"{"choices":[{"message":{"content":"ok"}}]}"#)
    }

    // --- DetectHardware ---

    #[tokio::test]
    async fn detect_hardware_reports_gpus() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let ctx = ctx_with(ops, RunMode::Install);
        let step = DetectHardware;
        assert!(step.is_check());
        assert!(step.prerequisites().is_empty());
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);
        assert!(out.message.contains("3 GPU(s)"), "{}", out.message);
        assert_eq!(out.log_tail.len(), 3);
        assert!(out.log_tail[1].contains("sm_60"));
    }

    #[tokio::test]
    async fn detect_hardware_fails_without_gpus() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(vec![]);
        let ctx = ctx_with(ops, RunMode::Install);
        let out = DetectHardware.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("no NVIDIA GPUs"));
    }

    #[test]
    fn detect_hardware_offers_driver_download_url() {
        assert_eq!(
            DetectHardware.help_url(),
            Some(crate::NVIDIA_DRIVER_DOWNLOAD_URL)
        );
        assert!(crate::NVIDIA_DRIVER_DOWNLOAD_URL.starts_with("https://"));
        let hint = DetectHardware.remediation_hint().unwrap();
        assert!(hint.contains(crate::NVIDIA_DRIVER_DOWNLOAD_URL));
    }

    #[tokio::test]
    async fn detect_hardware_rejects_old_driver() {
        let ops = Arc::new(FakeSystemOps::new());
        let mut gpus = test_gpus();
        for g in gpus.iter_mut() {
            g.driver_version = "471.11".to_string();
        }
        ops.with_gpus(gpus);
        let ctx = ctx_with(ops, RunMode::Install);
        let out = DetectHardware.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("too old"), "{}", out.message);
    }

    #[tokio::test]
    async fn detect_hardware_probe_error_fails() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpu_error("nvidia-smi not found");
        let ctx = ctx_with(ops, RunMode::Install);
        let out = DetectHardware.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
    }

    // --- InstallLlamaCpp ---

    #[tokio::test]
    async fn install_llamacpp_happy_path_and_transparency() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin();
        script_pin_downloads(&ops, &pin);
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = InstallLlamaCpp::with_pin(pin.clone());

        // Transparency: planned commands equal what will run — BOTH
        // archive downloads, binaries first.
        let planned = step.planned_commands(&ctx);
        assert_eq!(planned.len(), 2);
        assert_eq!(planned[0].argv[0], "download");
        assert_eq!(planned[0].argv[1], pin.download_url());
        assert!(
            planned[0].argv[1].contains("cuda-12.4"),
            "{:?}",
            planned[0].argv
        );
        assert_eq!(planned[1].argv[0], "download");
        assert_eq!(planned[1].argv[1], pin.runtime_download_url());

        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("b9999"));

        // Both archives downloaded + extracted; marker records both.
        let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
        let runtime_archive = PathBuf::from("/data/llamacpp").join(&pin.runtime.asset);
        assert!(ops.file_bytes(&archive).is_some());
        assert!(ops.file_bytes(&runtime_archive).is_some());
        assert_eq!(ops.downloads_made().len(), 2);
        let marker_bytes = ops
            .file_bytes(&PathBuf::from("/data/llamacpp/.installed.json"))
            .expect("marker written");
        let marker: serde_json::Value =
            serde_json::from_slice(&marker_bytes).expect("marker is JSON");
        assert_eq!(marker["tag"], "b9999");
        assert_eq!(marker["asset"], pin.asset);
        assert_eq!(marker["sha256"], pin.sha256);
        assert_eq!(marker["runtime_asset"], pin.runtime.asset);
        assert_eq!(marker["runtime_sha256"], pin.runtime.sha256);
        assert_eq!(ops.extractions().len(), 2);
        assert_eq!(ops.extractions()[0].0, archive);
        assert_eq!(ops.extractions()[1].0, runtime_archive);
    }

    #[tokio::test]
    async fn install_llamacpp_skips_redownload_when_verified() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin();
        let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
        ops.with_file(archive.clone(), b"fake-zip-bytes".to_vec());
        let runtime_archive = PathBuf::from("/data/llamacpp").join(&pin.runtime.asset);
        ops.with_file(runtime_archive, b"fake-cudart-bytes".to_vec());
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);
        assert!(ops.downloads_made().is_empty(), "no download needed");
        assert!(out
            .log_tail
            .iter()
            .any(|l| l.contains("already present with matching checksum")));
    }

    #[tokio::test]
    async fn install_llamacpp_redownloads_only_the_missing_archive() {
        // Binaries archive already verified on disk; the runtime archive
        // is missing, so exactly one download happens (the runtime one).
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin();
        let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
        ops.with_file(archive, b"fake-zip-bytes".to_vec());
        script_pin_downloads(&ops, &pin);
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallLlamaCpp::with_pin(pin.clone())
            .run(&ctx)
            .await
            .unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let made = ops.downloads_made();
        assert_eq!(made.len(), 1, "{made:?}");
        assert_eq!(made[0].0, pin.runtime_download_url());
    }

    #[tokio::test]
    async fn install_llamacpp_fails_closed_without_pin() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let ctx = ctx_with(ops, RunMode::Install);
        let out = InstallLlamaCpp::unconfigured().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("no llama.cpp build pinned"));
        // The message must state the real cause (a packaging error in the
        // build) and must NOT direct the operator to configure
        // updates.components — that path cannot supply a pin.
        let detail = out.log_tail.join("\n");
        assert!(detail.contains("packaging error"), "{detail}");
        assert!(!detail.contains("configure updates.components"), "{detail}");
    }

    #[tokio::test]
    async fn install_llamacpp_rejects_cuda13_on_pascal() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus()); // includes P100s
        let pin = LlamaCppPin {
            tag: "b9999".to_string(),
            asset: "llama-b9999-bin-win-cuda-13.0-x64.zip".to_string(),
            sha256: sha256_hex(b"x"),
            runtime: LlamaCppAsset {
                asset: "cudart-llama-bin-win-cuda-12.4-x64.zip".to_string(),
                sha256: sha256_hex(b"y"),
            },
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("cuda-13"), "{}", out.message);
        assert!(ops.downloads_made().is_empty(), "no download attempted");
    }

    #[tokio::test]
    async fn install_llamacpp_rejects_cuda13_runtime_on_pascal() {
        // The Pascal rule covers BOTH archives: a cuda-12.4 binaries
        // archive paired with a cuda-13 runtime archive is still refused,
        // and the error names the runtime asset.
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus()); // includes P100s
        let pin = LlamaCppPin {
            tag: "b9999".to_string(),
            asset: "llama-b9999-bin-win-cuda-12.4-x64.zip".to_string(),
            sha256: sha256_hex(b"x"),
            runtime: LlamaCppAsset {
                asset: "cudart-llama-bin-win-cuda-13.0-x64.zip".to_string(),
                sha256: sha256_hex(b"y"),
            },
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("cuda-13"), "{}", out.message);
        assert!(
            out.message
                .contains("cudart-llama-bin-win-cuda-13.0-x64.zip"),
            "{}",
            out.message
        );
        assert!(ops.downloads_made().is_empty(), "no download attempted");
    }

    #[tokio::test]
    async fn install_llamacpp_allows_cuda13_without_pascal() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(vec![test_gpus()[0].clone()]); // A4000 only
        let pin = LlamaCppPin {
            tag: "b9999".to_string(),
            asset: "llama-b9999-bin-win-cuda-13.0-x64.zip".to_string(),
            sha256: sha256_hex(b"fake-zip-bytes"),
            runtime: LlamaCppAsset {
                asset: "cudart-llama-bin-win-cuda-12.4-x64.zip".to_string(),
                sha256: sha256_hex(b"fake-cudart-bytes"),
            },
        };
        script_pin_downloads(&ops, &pin);
        let ctx = ctx_with(ops, RunMode::Install);
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);
    }

    #[tokio::test]
    async fn install_llamacpp_checksum_mismatch_fails() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin();
        ops.with_download(&pin.download_url(), b"tampered-bytes".to_vec());
        let ctx = ctx_with(ops, RunMode::Install);
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("checksum mismatch"));
    }

    #[tokio::test]
    async fn install_llamacpp_audit_reverifies_and_detects_tamper() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin();
        script_pin_downloads(&ops, &pin);
        let install_ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = InstallLlamaCpp::with_pin(pin.clone());
        assert_eq!(step.run(&install_ctx).await.unwrap().state, StepState::Done);

        // Audit passes on the pristine install.
        let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = step.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);

        // Tamper with the archive: audit must fail with a remediation hint.
        let archive = PathBuf::from("/data/llamacpp").join(&pin.asset);
        ops.with_file(archive, b"tampered".to_vec());
        let out = step.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("checksum mismatch"));
        assert!(step.remediation_hint().unwrap().contains("re-download"));
    }

    #[tokio::test]
    async fn install_llamacpp_audit_fails_on_runtime_checksum_mismatch() {
        // Audit verifies BOTH archives: a tampered runtime archive fails
        // even when the binaries archive is pristine, and the message
        // names the runtime archive.
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin();
        script_pin_downloads(&ops, &pin);
        let install_ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = InstallLlamaCpp::with_pin(pin.clone());
        assert_eq!(step.run(&install_ctx).await.unwrap().state, StepState::Done);

        let runtime_archive = PathBuf::from("/data/llamacpp").join(&pin.runtime.asset);
        ops.with_file(runtime_archive, b"tampered-cudart".to_vec());
        let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = step.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("checksum mismatch"), "{}", out.message);
        assert!(out.message.contains(&pin.runtime.asset), "{}", out.message);
    }

    #[test]
    fn default_pin_is_verified_b11658_cuda124_pair() {
        let pin = LlamaCppPin::default_pin();
        assert_eq!(pin.tag, "b11658");
        assert_eq!(pin.asset, "llama-b11658-bin-win-cuda-12.4-x64.zip");
        assert_eq!(
            pin.sha256,
            "4268543cfffee11e62dbe1fb4ee907da508c2a70de7b845ed7b338bf8cabd76d"
        );
        assert_eq!(pin.runtime.asset, "cudart-llama-bin-win-cuda-12.4-x64.zip");
        assert_eq!(
            pin.runtime.sha256,
            "8c79a9b226de4b3cacfd1f83d24f962d0773be79f1e7b75c6af4ded7e32ae1d6"
        );
        for sha in [&pin.sha256, &pin.runtime.sha256] {
            assert_eq!(sha.len(), 64, "{sha}");
            assert!(
                sha.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{sha}"
            );
        }
        for name in [&pin.asset, &pin.runtime.asset] {
            assert!(name.contains("cuda-12.4"), "{name}");
            assert!(!name.contains("cuda-13"), "{name}");
        }
        assert_eq!(
            pin.download_url(),
            "https://github.com/ggml-org/llama.cpp/releases/download/b11658/llama-b11658-bin-win-cuda-12.4-x64.zip"
        );
        assert_eq!(
            pin.runtime_download_url(),
            "https://github.com/ggml-org/llama.cpp/releases/download/b11658/cudart-llama-bin-win-cuda-12.4-x64.zip"
        );
        // Regression for the shipped defect: the default step list must
        // carry the pin (it previously shipped unconfigured and step 2
        // could never pass).
        let ctx = WizardContext::new(
            AppConfig::default_config(),
            PathBuf::from("/data"),
            RunMode::Install,
            Arc::new(FakeSystemOps::new()),
        );
        let step = default_steps()
            .into_iter()
            .find(|s| s.id() == STEP_INSTALL_LLAMACPP)
            .expect("llamacpp step present in default steps");
        let planned = step.planned_commands(&ctx);
        assert_eq!(planned.len(), 2, "{planned:?}");
        assert!(
            planned[0].argv[1].contains("b11658"),
            "{:?}",
            planned[0].argv
        );
        assert!(
            planned[1].argv[1].contains("cudart"),
            "{:?}",
            planned[1].argv
        );
    }

    /// Context whose config pins `updates.components.llamacpp` to the
    /// given version (or leaves the component unpinned when `None`).
    fn ctx_with_llamacpp_pin(
        ops: Arc<FakeSystemOps>,
        mode: RunMode,
        pinned_version: Option<&str>,
    ) -> WizardContext {
        let mut cfg = test_config_with_models();
        cfg.updates.components.insert(
            "llamacpp".to_string(),
            ComponentUpdate {
                pinned_version: pinned_version.map(str::to_string),
                auto_update: false,
                last_checked: None,
            },
        );
        WizardContext::new(cfg, PathBuf::from("/data"), mode, ops)
    }

    #[tokio::test]
    async fn install_llamacpp_config_version_mismatch_fails_closed() {
        // The config names a version this build carries no verified pin
        // for: refused before any download, naming both versions.
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin(); // built-in tag b9999
        script_pin_downloads(&ops, &pin);
        let ctx = ctx_with_llamacpp_pin(ops.clone(), RunMode::Install, Some("b1111"));
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("b1111"), "{}", out.message);
        assert!(out.message.contains("b9999"), "{}", out.message);
        assert!(
            ops.downloads_made().is_empty(),
            "refused before any download"
        );
    }

    #[tokio::test]
    async fn install_llamacpp_config_version_match_proceeds() {
        // A config pin naming exactly the built-in tag is a no-op guard:
        // the step proceeds with the verified pin.
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let pin = test_pin(); // built-in tag b9999
        script_pin_downloads(&ops, &pin);
        let ctx = ctx_with_llamacpp_pin(ops.clone(), RunMode::Install, Some("b9999"));
        let out = InstallLlamaCpp::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert_eq!(ops.downloads_made().len(), 2);
    }

    // --- ConfigureTensorSplit ---

    #[tokio::test]
    async fn compute_splits_sums_to_one() {
        let splits = compute_splits(&[16384, 16384, 16384]).unwrap();
        assert_eq!(splits.len(), 3);
        let sum: f32 = splits.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "{splits:?}");
        // Uneven VRAM: proportional shares.
        let splits = compute_splits(&[24576, 8192]).unwrap();
        assert!((splits[0] - 0.75).abs() < 1e-6, "{splits:?}");
        assert!((splits[1] - 0.25).abs() < 1e-6);
        // Zero VRAM is an error, not a silent [0,0].
        assert!(compute_splits(&[0, 0]).is_err());
    }

    #[test]
    fn compute_splits_never_negative_and_always_sums_to_one() {
        // Deterministic pseudo-random sweep: adversarial VRAM mixes over
        // 1..=40 GPUs must never produce a negative share or a bad sum.
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed >> 33
        };
        for _ in 0..2000 {
            let n = 1 + (next() % 40) as usize;
            let vrams: Vec<u64> = (0..n).map(|_| 1024 + next() % 49152).collect();
            let splits = compute_splits(&vrams).unwrap();
            assert_eq!(splits.len(), n);
            for s in &splits {
                assert!(
                    (0.0..=1.0).contains(s),
                    "split out of range: {splits:?} from {vrams:?}"
                );
            }
            let sum: f32 = splits.iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-4,
                "sum != 1.0: {splits:?} from {vrams:?}"
            );
        }
        // Single GPU owns the whole split.
        assert_eq!(compute_splits(&[16384]).unwrap(), vec![1.0]);
        // Empty input is zero total VRAM, not a panic.
        assert!(compute_splits(&[]).is_err());
    }

    // --- assign_splits (pure) ---

    fn gi_b(n: f64) -> u64 {
        (n * 1_073_741_824.0) as u64
    }

    #[test]
    fn assign_splits_single_gpu_assigns_nothing() {
        let inputs = vec![SplitInput {
            model_bytes: gi_b(5.0),
            existing: None,
            assign: true,
        }];
        let out = assign_splits(&inputs, &[16384]);
        assert_eq!(out, vec![None]);
    }

    #[test]
    fn assign_splits_fitting_model_goes_to_gpu_with_most_room() {
        // Accounting-only entry occupies 4 GB of GPU 0 (its unset
        // split means llama.cpp puts it there today); the assignable
        // 6 GB model no longer fits GPU 0's remaining placeable space
        // and must go entirely on GPU 1.
        let inputs = vec![
            SplitInput {
                model_bytes: gi_b(4.0),
                existing: None,
                assign: false,
            },
            SplitInput {
                model_bytes: gi_b(6.0),
                existing: None,
                assign: true,
            },
        ];
        let out = assign_splits(&inputs, &[8192, 16384]);
        assert_eq!(out[0], None);
        assert_eq!(out[1], Some((vec![0.0, 1.0], SplitMode::Layer)));
    }

    #[test]
    fn assign_splits_oversized_model_spreads_proportionally() {
        let inputs = vec![SplitInput {
            model_bytes: gi_b(40.0),
            existing: None,
            assign: true,
        }];
        let out = assign_splits(&inputs, &[16384, 16384]);
        assert_eq!(out[0], Some((vec![0.5, 0.5], SplitMode::Layer)));
    }

    #[test]
    fn assign_splits_keeps_valid_existing_and_deducts_it() {
        // Existing [0.5, 0.5] of a 20 GB model leaves ~5.0 GB
        // placeable on each 16 GiB GPU; the new 10 GB model fits
        // neither and spreads over the remaining room evenly.
        let inputs = vec![
            SplitInput {
                model_bytes: gi_b(20.0),
                existing: Some(vec![0.5, 0.5]),
                assign: false,
            },
            SplitInput {
                model_bytes: gi_b(10.0),
                existing: None,
                assign: true,
            },
        ];
        let out = assign_splits(&inputs, &[16384, 16384]);
        assert_eq!(out[0], Some((vec![0.5, 0.5], SplitMode::Layer)));
        assert_eq!(out[1], Some((vec![0.5, 0.5], SplitMode::Layer)));
    }

    #[test]
    fn assign_splits_stale_length_existing_is_recomputed() {
        let inputs = vec![SplitInput {
            model_bytes: gi_b(5.0),
            existing: Some(vec![1.0]),
            assign: true,
        }];
        let out = assign_splits(&inputs, &[16384, 16384]);
        let (split, _) = out[0].clone().expect("assigned");
        assert_eq!(split.len(), 2);
        let sum: f32 = split.iter().sum();
        assert!((sum - 1.0).abs() < 1e-4);
    }

    #[test]
    fn assign_splits_largest_first_packing() {
        // Two 10 GB models, two 16 GiB GPUs (14.4 GiB placeable
        // each): each model gets its own GPU rather than the second
        // being spread around the first's leftovers.
        let inputs = vec![
            SplitInput {
                model_bytes: gi_b(10.0),
                existing: None,
                assign: true,
            },
            SplitInput {
                model_bytes: gi_b(10.0),
                existing: None,
                assign: true,
            },
        ];
        let out = assign_splits(&inputs, &[16384, 16384]);
        assert_eq!(out[0], Some((vec![1.0, 0.0], SplitMode::Layer)));
        assert_eq!(out[1], Some((vec![0.0, 1.0], SplitMode::Layer)));
    }

    // --- ConfigureTensorSplit step ---

    #[tokio::test]
    async fn tensor_split_install_persists_splits_and_audit_validates() {
        let data_dir = stamp_test_dir("splits");
        let config_path = data_dir.join("config.json");
        let mut config = test_config_with_models();
        // The shared test config's backend model files do not resolve
        // to its two registered models (save() would reject it);
        // point each backend at one of them.
        for b in config.backends.iter_mut() {
            b.model_file = if b.id == "coder-fast" {
                PathBuf::from("models/qwen2.5-coder-7b.gguf")
            } else {
                PathBuf::from("models/qwen3-8b.gguf")
            };
        }
        manager_config::save(&config, &config_path).unwrap();
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_gpus(test_gpus());
        let step = ConfigureTensorSplit;
        assert!(step
            .planned_commands(&WizardContext::new(
                config.clone(),
                data_dir.clone(),
                RunMode::Install,
                ops.clone(),
            ))
            .is_empty());

        // Audit against the unsplit config fails: this is the state
        // Pete's machine was in — backends configured, no placement.
        let audit_ctx = WizardContext::new(
            config.clone(),
            data_dir.clone(),
            RunMode::Audit,
            ops.clone(),
        );
        let out = step.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(out.message.contains("no tensor split"), "{}", out.message);

        // Install assigns and persists into config.json.
        let install_ctx = WizardContext::new(
            config.clone(),
            data_dir.clone(),
            RunMode::Install,
            ops.clone(),
        );
        let out = step.run(&install_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let saved = manager_config::load(&config_path).unwrap();
        manager_config::validate(&saved).unwrap();
        let enabled: Vec<_> = saved.backends.iter().filter(|b| b.enabled).collect();
        assert!(!enabled.is_empty());
        for b in &enabled {
            assert_eq!(b.flags.tensor_split.len(), 3, "{}: {:?}", b.id, b.flags);
            let sum: f32 = b.flags.tensor_split.iter().sum();
            assert!((sum - 1.0).abs() < 1e-3, "{} sums to {sum}", b.id);
        }

        // Audit against the persisted config passes.
        let audit_ctx = WizardContext::new(saved, data_dir.clone(), RunMode::Audit, ops.clone());
        let out = step.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    // --- DownloadModels ---

    #[tokio::test]
    async fn download_models_install_and_verify() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_download(
            "https://example.com/qwen3-8b.gguf",
            b"gguf-8b-bytes".to_vec(),
        );
        ops.with_download(
            "https://example.com/qwen2.5-coder-7b.gguf",
            b"gguf-7b-bytes".to_vec(),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = DownloadModels;
        assert_eq!(step.planned_commands(&ctx).len(), 2);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("2/2"));
        assert_eq!(ops.downloads_made().len(), 2);
    }

    #[tokio::test]
    async fn download_models_skips_disabled_models() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_download(
            "https://example.com/qwen3-8b.gguf",
            b"gguf-8b-bytes".to_vec(),
        );
        ops.with_download(
            "https://example.com/qwen2.5-coder-7b.gguf",
            b"gguf-7b-bytes".to_vec(),
        );
        let mut config = test_config_with_models();
        config.models[1].enabled = false;
        let ctx = WizardContext::new(
            config,
            PathBuf::from("/data"),
            RunMode::Install,
            ops.clone(),
        );
        let step = DownloadModels;
        // Planned commands cover enabled models only.
        assert_eq!(step.planned_commands(&ctx).len(), 1);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("1/1"), "{}", out.message);
        assert_eq!(ops.downloads_made().len(), 1);
    }

    #[tokio::test]
    async fn download_models_none_enabled_is_a_no_op() {
        let ops = Arc::new(FakeSystemOps::new());
        let mut config = test_config_with_models();
        for m in &mut config.models {
            m.enabled = false;
        }
        let ctx = WizardContext::new(
            config,
            PathBuf::from("/data"),
            RunMode::Install,
            ops.clone(),
        );
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("no models enabled"), "{}", out.message);
        assert_eq!(ops.downloads_made().len(), 0);
    }

    #[tokio::test]
    async fn download_models_missing_url_fails_closed() {
        let ops = Arc::new(FakeSystemOps::new());
        let mut config = test_config_with_models();
        config.models[0].source_url.clear();
        let ctx = WizardContext::new(config, PathBuf::from("/data"), RunMode::Install, ops);
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("no source URL"));
    }

    #[tokio::test]
    async fn download_models_audit_detects_corruption() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_download(
            "https://example.com/qwen3-8b.gguf",
            b"gguf-8b-bytes".to_vec(),
        );
        ops.with_download(
            "https://example.com/qwen2.5-coder-7b.gguf",
            b"gguf-7b-bytes".to_vec(),
        );
        let install_ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = DownloadModels;
        assert_eq!(step.run(&install_ctx).await.unwrap().state, StepState::Done);

        let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
        assert_eq!(step.run(&audit_ctx).await.unwrap().state, StepState::Done);

        // Corrupt one file on disk.
        ops.with_file(
            PathBuf::from("/data/models/qwen3-8b.gguf"),
            b"corrupted".to_vec(),
        );
        let out = step.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("checksum mismatch"));
        assert!(step.remediation_hint().unwrap().contains("re-download"));
        // Audit performed no downloads.
        assert_eq!(ops.downloads_made().len(), 2, "audit added no downloads");
    }

    // --- DownloadModels: verified_at stamping ---

    /// Config fixture that passes `manager_config::save` validation.
    /// The shared fixture enables every backend, but its backends point
    /// at model files absent from the replaced models list, which
    /// save() rejects. The stamp tests persist the config, so they
    /// disable the backends (DownloadModels ignores backends).
    fn stamp_test_config() -> AppConfig {
        let mut c = test_config_with_models();
        for b in c.backends.iter_mut() {
            b.enabled = false;
        }
        c
    }

    /// Unique real temp data dir for the stamp tests: the stamp
    /// targets `<data_dir>/config.json` on the real filesystem.
    fn stamp_test_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("llm-wiz-stamp-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn download_models_install_stamps_verified_at() {
        let data_dir = stamp_test_dir("install");
        let config_path = data_dir.join("config.json");
        // Nothing persisted yet (first run): the stamp falls back to
        // the context config and creates the persisted file.
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_download(
            "https://example.com/qwen3-8b.gguf",
            b"gguf-8b-bytes".to_vec(),
        );
        ops.with_download(
            "https://example.com/qwen2.5-coder-7b.gguf",
            b"gguf-7b-bytes".to_vec(),
        );
        let ctx = WizardContext::new(stamp_test_config(), data_dir.clone(), RunMode::Install, ops);
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");

        let persisted = manager_config::load(&config_path).unwrap();
        assert_eq!(persisted.models.len(), 2);
        for m in &persisted.models {
            assert!(m.verified_at.is_some(), "{} verified but not stamped", m.id);
        }
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn download_models_audit_stamps_verified_at() {
        let data_dir = stamp_test_dir("audit");
        let config_path = data_dir.join("config.json");
        // A persisted config with no stamps yet: audit loads it as
        // the stamp base.
        let config = stamp_test_config();
        manager_config::save(&config, &config_path).unwrap();

        // Both model files already installed with matching bytes.
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_file(
            data_dir.join("models/qwen3-8b.gguf"),
            b"gguf-8b-bytes".to_vec(),
        );
        ops.with_file(
            data_dir.join("models/qwen2.5-coder-7b.gguf"),
            b"gguf-7b-bytes".to_vec(),
        );
        let ctx = WizardContext::new(config, data_dir.clone(), RunMode::Audit, ops);
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("2/2"), "{out:?}");

        let persisted = manager_config::load(&config_path).unwrap();
        for m in &persisted.models {
            assert!(m.verified_at.is_some(), "{} audited but not stamped", m.id);
        }
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn download_models_mismatch_is_not_stamped() {
        let data_dir = stamp_test_dir("mismatch");
        let config_path = data_dir.join("config.json");
        let config = stamp_test_config();
        manager_config::save(&config, &config_path).unwrap();

        let ops = Arc::new(FakeSystemOps::new());
        ops.with_download(
            "https://example.com/qwen3-8b.gguf",
            b"gguf-8b-bytes".to_vec(),
        );
        // The second model's bytes do not match its pinned hash.
        ops.with_download(
            "https://example.com/qwen2.5-coder-7b.gguf",
            b"tampered-bytes".to_vec(),
        );
        let ctx = WizardContext::new(config, data_dir.clone(), RunMode::Install, ops);
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("checksum mismatch"), "{out:?}");

        let persisted = manager_config::load(&config_path).unwrap();
        let good = persisted
            .models
            .iter()
            .find(|m| m.id == "qwen3-8b")
            .unwrap();
        assert!(
            good.verified_at.is_some(),
            "model verified earlier in the same run is stamped"
        );
        let bad = persisted
            .models
            .iter()
            .find(|m| m.id == "qwen2.5-coder-7b")
            .unwrap();
        assert!(
            bad.verified_at.is_none(),
            "mismatching model must not be stamped"
        );
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    // --- StartGateway ---

    #[tokio::test]
    async fn start_gateway_install_spawns_and_healthchecks() {
        let _guard = PathGuard::set_to(&path_dir_with_uv(true));
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let planned = step.planned_commands(&ctx);
        assert_eq!(planned.len(), 7);
        // Probes first, then the pinned venv creation, pip, the spawn,
        // and the firewall reconciliation (show rule, elevated script).
        assert!(planned[2].argv[0].ends_with("uv"));
        assert!(planned[2].argv.contains(&"--python".to_string()));
        assert!(planned[2]
            .argv
            .contains(&crate::GATEWAY_PYTHON_VERSION.to_string()));
        assert!(planned[2].argv.contains(&"--clear".to_string()));

        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        // No venv exists yet, so the probes execute nothing: venv + pip
        // install ran; gateway spawned with the right port.
        let executed = ops.executed_commands();
        assert_eq!(executed[0], planned[2].argv);
        assert_eq!(executed[1], planned[3].argv);
        let spawns = ops.spawns();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0], planned[4].argv);
        assert!(spawns[0].contains(&"4000".to_string()));
        // config.yaml generated from app state.
        let cfg = ops
            .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
            .expect("config written");
        let text = String::from_utf8(cfg).unwrap();
        assert!(text.contains("8081"), "{text}");
    }

    #[tokio::test]
    async fn start_gateway_recreates_venv_on_wrong_python_version() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        // An existing venv whose interpreter reports Python 3.15 — the
        // unpinned failure mode (hiredis has no cp315 wheel). The step
        // must recreate it pinned instead of failing at `uv venv`.
        ops.with_file(step.venv_python(&ctx), Vec::new());
        let probe = step.version_probe_command(&ctx);
        let probe_argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
        ops.with_command(
            &probe_argv,
            crate::CommandOutput {
                exit_code: 0,
                stdout: "3.15\n".to_string(),
                stderr: String::new(),
            },
        );
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("recreating it with Python")),
            "{out:?}"
        );
        let executed = ops.executed_commands();
        let venv_cmd = executed
            .iter()
            .find(|a| a.iter().any(|x| x == "venv"))
            .expect("venv recreated");
        assert!(venv_cmd.contains(&"--clear".to_string()));
        assert!(venv_cmd.contains(&crate::GATEWAY_PYTHON_VERSION.to_string()));
        assert!(
            executed.iter().any(|a| a.iter().any(|x| x == "install")),
            "pip install ran: {executed:?}"
        );
        assert_eq!(ops.spawns().len(), 1);
    }

    #[tokio::test]
    async fn start_gateway_reuses_current_venv_with_litellm() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        // Venv already on the pinned Python; the version probe reports
        // it and the import probe succeeds by default — no venv or pip
        // command may run, but the gateway still spawns.
        ops.with_file(step.venv_python(&ctx), Vec::new());
        let probe = step.version_probe_command(&ctx);
        let probe_argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
        ops.with_command(
            &probe_argv,
            crate::CommandOutput {
                exit_code: 0,
                stdout: format!("{}\n", crate::GATEWAY_PYTHON_VERSION),
                stderr: String::new(),
            },
        );
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let executed = ops.executed_commands();
        assert!(
            executed
                .iter()
                .all(|a| !a.iter().any(|x| x == "venv" || x == "install")),
            "no venv/pip commands may run: {executed:?}"
        );
        assert_eq!(ops.spawns().len(), 1);
    }

    #[tokio::test]
    async fn start_gateway_spawn_uses_venv_python_and_captures_logs() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let cmds = ops.spawn_cmds();
        assert_eq!(cmds.len(), 1);
        // The venv interpreter directly, running the console
        // script's entry point via -c: never the litellm.exe launcher
        // (its child python.exe gets a visible console window) and
        // never `python -m litellm` (no __main__ in the package).
        assert!(
            cmds[0].argv[0].ends_with("python")
                || cmds[0].argv[0].ends_with("python.exe")
                || cmds[0].argv[0].ends_with("pythonw.exe"),
            "{:?}",
            cmds[0].argv
        );
        assert_eq!(cmds[0].argv[1], "-c");
        assert!(
            cmds[0].argv[2].contains("from litellm import run_server"),
            "{:?}",
            cmds[0].argv
        );
        assert!(
            !cmds[0].argv.iter().any(|a| a == "-m"),
            "{:?}",
            cmds[0].argv
        );
        // Output capture is wired to the gateway log files.
        assert_eq!(
            cmds[0].stdout_log.as_deref(),
            Some(std::path::Path::new("/data/logs/gateway.stdout.log"))
        );
        assert_eq!(
            cmds[0].stderr_log.as_deref(),
            Some(std::path::Path::new("/data/logs/gateway.stderr.log"))
        );
        // A master key was generated, stored, and injected into the
        // gateway's environment (current LiteLLM refuses to boot
        // without one).
        let key = cmds[0]
            .env
            .get("LITELLM_MASTER_KEY")
            .expect("master key env set");
        // UTF-8 stdio is forced: with redirected output, cp1252
        // cannot encode LiteLLM's startup banner and startup fails.
        assert_eq!(
            cmds[0].env.get("PYTHONIOENCODING").map(String::as_str),
            Some("utf-8")
        );
        assert_eq!(cmds[0].env.get("PYTHONUTF8").map(String::as_str), Some("1"));
        assert!(key.starts_with("sk-") && key.len() > 32, "{key}");
        assert_eq!(
            ops.credential_value(crate::LITELLM_MASTER_KEY_CRED_TARGET)
                .as_deref(),
            Some(key.as_str()),
            "stored credential matches the injected key"
        );
    }

    #[tokio::test]
    async fn start_gateway_fails_fast_when_process_exits() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        // The spawned gateway exits immediately; its captured stderr
        // carries the interpreter error the operator needs to see.
        ops.with_dead_spawn();
        ops.with_file(
            PathBuf::from("/data/logs/gateway.stderr.log"),
            b"C:\\venv\\python.exe: No module named litellm.__main__; 'litellm' is a package and cannot be directly executed\n".to_vec(),
        );
        let step = StartGateway {
            health_retries: 30,
            health_interval_ms: 1,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        // Detected on the first probe, not after the full budget.
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("exited during startup (probe attempt 1)")),
            "{out:?}"
        );
        // The captured gateway stderr tail rides in the outcome.
        assert!(
            out.log_tail.iter().any(|l| l.contains("No module named")),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn start_gateway_install_stops_previous_and_records_pid() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        // A previous gateway (pid 7777) is recorded and still running.
        ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
        ops.with_live_pid(7777);
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        // The old process was terminated before the replacement spawned,
        // and the pid file now records the new spawn (fake pid 4242).
        assert_eq!(ops.killed_pids(), vec![7777]);
        assert_eq!(ops.spawns().len(), 1);
        let pid_file = ops
            .file_bytes(&PathBuf::from("/data/gateway.pid"))
            .expect("pid file written");
        assert_eq!(String::from_utf8(pid_file).unwrap(), "4242");
    }

    #[tokio::test]
    async fn start_gateway_install_fails_when_previous_cannot_be_stopped() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
        ops.with_live_pid(7777);
        ops.with_kill_failure();
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(
            out.message.contains("could not stop the previous gateway"),
            "{out:?}"
        );
        // No duplicate was spawned over the still-running old process.
        assert!(ops.spawns().is_empty());
    }

    #[tokio::test]
    async fn start_gateway_audit_does_not_stop_recorded_gateway() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok("{}"),
        );
        ops.with_file(PathBuf::from("/data/gateway.pid"), b"7777".to_vec());
        ops.with_live_pid(7777);
        let ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = StartGateway::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);
        assert!(ops.killed_pids().is_empty(), "audit kills nothing");
        assert!(ops.spawns().is_empty(), "audit spawns nothing");
    }

    fn gateway_ctx_with_openrouter(ops: Arc<FakeSystemOps>, mode: RunMode) -> WizardContext {
        let mut cfg = test_config_with_models();
        cfg.gateway.openrouter.enabled = true;
        cfg.gateway.openrouter.cred_ref = Some("LocalLLM/OpenRouter".to_string());
        cfg.gateway.openrouter.model_allowlist = vec!["openai/gpt-4o-mini".to_string()];
        WizardContext::new(cfg, PathBuf::from("/data"), mode, ops)
    }

    /// Script the bootstrapped uv in the fake fs so gateway tests do
    /// not need the PATH-mutating PathGuard (parallel tests share the
    /// process PATH; these tests must not race the ones that use it).
    fn with_bundled_uv(ops: &FakeSystemOps) {
        let name = if cfg!(target_os = "windows") {
            "uv.exe"
        } else {
            "uv"
        };
        ops.with_file(PathBuf::from("/data/bin").join(name), b"fake-uv".to_vec());
    }

    #[tokio::test]
    async fn start_gateway_injects_openrouter_key_from_credential() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_credential_value("LocalLLM/OpenRouter", "sk-test-value");
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx = gateway_ctx_with_openrouter(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        // The plan records the variable but never the value.
        let planned = step.planned_commands(&ctx);
        let plan_spawn = planned
            .iter()
            .find(|c| c.argv.iter().any(|a| a == "--config"))
            .expect("gateway spawn planned");
        assert_eq!(
            plan_spawn.env.get("OPENROUTER_API_KEY").map(String::as_str),
            Some("<set from credential manager>")
        );
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        // The spawned gateway carries the key in its environment.
        let envs = ops.spawn_envs();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].get("OPENROUTER_API_KEY").map(String::as_str),
            Some("sk-test-value")
        );
        // The configured bind on the spawn argv: the shipped default
        // is 0.0.0.0 (LAN-reachable), per Pete's 2026-10-15 decision.
        let spawns = ops.spawns();
        let host = spawns[0]
            .windows(2)
            .find(|w| w[0] == "--host")
            .expect("--host present");
        assert_eq!(host[1], "0.0.0.0");
        // The key never lands in the generated YAML.
        let cfg = ops
            .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
            .expect("config written");
        let text = String::from_utf8(cfg).unwrap();
        assert!(!text.contains("sk-test-value"), "{text}");
    }

    /// Firewall reconciliation, add path: LAN-bound (the default)
    /// with no rule present — the step elevates once to run the add
    /// script, and a successful add leaves the Done message clean.
    #[tokio::test]
    async fn start_gateway_adds_firewall_rule_when_missing() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        ops.with_command_failure(
            &[
                "netsh",
                "advfirewall",
                "firewall",
                "show",
                "rule",
                "name=Local LLM Gateway (TCP 4000)",
            ],
            1,
            "No rules match the specified criteria.",
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(!out.message.contains("firewall"), "{out:?}");
        let elevated = ops.elevated_calls();
        assert_eq!(elevated.len(), 1, "{elevated:?}");
        let script = ops
            .file_bytes(&PathBuf::from("/data/gateway/firewall.cmd"))
            .expect("firewall script written");
        let text = String::from_utf8(script).unwrap();
        assert!(
            text.contains("add rule name=\"Local LLM Gateway (TCP 4000)\""),
            "{text}"
        );
        assert!(text.contains("localport=4000"), "{text}");
        assert!(text.contains("remoteip=localsubnet"), "{text}");
    }

    /// Firewall reconciliation, remove path: loopback-bound with the
    /// rule present (the fake's default command success) — one
    /// elevated delete.
    #[tokio::test]
    async fn start_gateway_removes_firewall_rule_when_loopback() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let mut config = test_config_with_models();
        config.gateway.bind_host = "127.0.0.1".to_string();
        let ctx = WizardContext::new(
            config,
            PathBuf::from("/data"),
            RunMode::Install,
            ops.clone(),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert_eq!(ops.elevated_calls().len(), 1);
        let script = ops
            .file_bytes(&PathBuf::from("/data/gateway/firewall.cmd"))
            .expect("firewall script written");
        let text = String::from_utf8(script).unwrap();
        assert!(
            text.contains("delete rule name=\"Local LLM Gateway (TCP 4000)\""),
            "{text}"
        );
        assert!(!text.contains("add rule"), "{text}");
    }

    /// Firewall reconciliation, no-op path: LAN-bound with the rule
    /// already present — no elevation, no script.
    #[tokio::test]
    async fn start_gateway_firewall_noop_when_rule_matches() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(ops.elevated_calls().is_empty());
        assert!(ops
            .file_bytes(&PathBuf::from("/data/gateway/firewall.cmd"))
            .is_none());
    }

    #[test]
    fn gateway_probe_host_maps_wildcards_to_loopback() {
        let mut config = test_config_with_models();
        assert_eq!(gateway_probe_host(&config), "127.0.0.1");
        config.gateway.bind_host = "::".to_string();
        assert_eq!(gateway_probe_host(&config), "127.0.0.1");
        config.gateway.bind_host = "127.0.0.1".to_string();
        assert_eq!(gateway_probe_host(&config), "127.0.0.1");
        config.gateway.bind_host = "192.168.1.50".to_string();
        assert_eq!(gateway_probe_host(&config), "192.168.1.50");
    }

    #[tokio::test]
    async fn start_gateway_warns_when_openrouter_credential_unreadable() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        // No credential scripted: credential_read returns None.
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx = gateway_ctx_with_openrouter(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        // The step still succeeds; the outcome carries the warning and
        // the spawn has no key.
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.message.contains("OpenRouter fallback is not armed"),
            "{out:?}"
        );
        let envs = ops.spawn_envs();
        assert_eq!(envs.len(), 1);
        assert!(!envs[0].contains_key("OPENROUTER_API_KEY"));
    }

    #[tokio::test]
    async fn start_gateway_fails_when_unhealthy() {
        let _guard = PathGuard::set_to(&path_dir_with_uv(true));
        let ops = Arc::new(FakeSystemOps::new());
        // No scripted health response → probes fail.
        let ctx = ctx_with(ops, RunMode::Install);
        let step = StartGateway {
            health_retries: 2,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("did not become healthy"));
    }

    #[tokio::test]
    async fn start_gateway_audit_probes_health() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok("{}"),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = StartGateway::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);
        assert!(ops.spawns().is_empty(), "audit spawns nothing");
    }

    #[test]
    fn find_uv_in_detects_binary() {
        let dir = path_dir_with_uv(true);
        let found = StartGateway::find_uv_in(std::slice::from_ref(&dir));
        assert!(found.is_some());
        let empty = path_dir_with_uv(false);
        assert!(StartGateway::find_uv_in(std::slice::from_ref(&empty)).is_none());
        assert!(StartGateway::find_uv_in(&[]).is_none());
    }

    #[tokio::test]
    async fn start_gateway_uses_bundled_uv_when_present() {
        // uv not on PATH, but the bootstrapped copy exists in the fake fs.
        let _guard = PathGuard::set_to(&path_dir_with_uv(false));
        let ops = Arc::new(FakeSystemOps::new());
        let name = if cfg!(target_os = "windows") {
            "uv.exe"
        } else {
            "uv"
        };
        ops.with_file(PathBuf::from("/data/bin").join(name), b"fake-uv".to_vec());
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let executed = ops.executed_commands();
        assert!(executed[0][0].ends_with(name), "{:?}", executed[0]);
        // No bootstrap download happened.
        assert!(ops.downloaded_to().is_empty());
    }

    #[tokio::test]
    async fn start_gateway_planned_shows_bootstrap_when_uv_missing() {
        let _guard = PathGuard::set_to(&path_dir_with_uv(false));
        let ctx = ctx_with(Arc::new(FakeSystemOps::new()), RunMode::Install);
        let planned = StartGateway::default().planned_commands(&ctx);
        if cfg!(target_os = "windows") {
            assert_eq!(planned.len(), 6);
            assert_eq!(planned[0].argv[0], "download");
            assert!(
                planned[0].argv[1].contains("astral-sh/uv"),
                "{:?}",
                planned[0].argv
            );
        } else {
            // No auto-bootstrap off Windows: uv path shown, run() fails
            // with install guidance instead.
            assert_eq!(planned.len(), 7);
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn start_gateway_fails_with_install_guidance_without_uv() {
        let _guard = PathGuard::set_to(&path_dir_with_uv(false));
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = ctx_with(ops, RunMode::Install);
        let out = StartGateway::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(
            out.message
                .contains("auto-bootstrap is only supported on Windows"),
            "{out:?}"
        );
    }

    // --- InstallLlamaCpp: VC++ redistributable ---

    #[test]
    fn vcredist_check_passes_off_windows() {
        // The probe is Windows-only; other platforms always pass.
        assert!(InstallLlamaCpp::vcredist_present(&PathBuf::from(
            "/nonexistent-system32"
        )));
    }

    #[test]
    fn llamacpp_offers_vcredist_download_url() {
        assert_eq!(
            InstallLlamaCpp::with_pin(test_pin()).help_url(),
            Some(crate::VC_REDIST_DOWNLOAD_URL)
        );
        assert!(crate::VC_REDIST_DOWNLOAD_URL.starts_with("https://"));
    }

    // --- DownloadModels: disk-space pre-check ---

    #[tokio::test]
    async fn download_models_fails_when_disk_full() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_head_length("https://example.com/qwen3-8b.gguf", 50_000_000_000);
        ops.with_head_length("https://example.com/qwen2.5-coder-7b.gguf", 40_000_000_000);
        ops.with_free_space(1_000_000);
        let ctx = ctx_with(ops, RunMode::Install);
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("insufficient disk space"), "{out:?}");
    }

    #[tokio::test]
    async fn download_models_proceeds_when_space_sufficient() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_head_length("https://example.com/qwen3-8b.gguf", 5_000_000_000);
        ops.with_head_length("https://example.com/qwen2.5-coder-7b.gguf", 4_000_000_000);
        ops.with_free_space(100_000_000_000);
        ops.with_download(
            "https://example.com/qwen3-8b.gguf",
            b"gguf-8b-bytes".to_vec(),
        );
        ops.with_download(
            "https://example.com/qwen2.5-coder-7b.gguf",
            b"gguf-7b-bytes".to_vec(),
        );
        let ctx = ctx_with(ops, RunMode::Install);
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
    }

    // --- VscodeExtensions ---

    #[tokio::test]
    async fn vscode_install_writes_config_without_clobbering() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_home(PathBuf::from("/home/test"));
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = VscodeExtensions::default();
        assert_eq!(step.planned_commands(&ctx).len(), 3);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.message.contains("Continue configured"),
            "{}",
            out.message
        );
        // The `code --version` probe plus three extension installs ran.
        assert_eq!(ops.executed_commands().len(), 4);
        // Canonical config.yaml + .env written (nothing existed).
        let canonical = ops
            .file_bytes(&PathBuf::from("/home/test/.continue/config.yaml"))
            .expect("canonical config.yaml written");
        let text = String::from_utf8(canonical).unwrap();
        assert!(text.contains("http://127.0.0.1:4000/v1"), "{text}");
        assert!(text.contains("planner"), "{text}");
        assert!(text.contains("${{ secrets.LITELLM_API_KEY }}"), "{text}");
        let env = ops
            .file_bytes(&PathBuf::from("/home/test/.continue/.env"))
            .expect(".env written");
        let env_text = String::from_utf8(env).unwrap();
        assert!(env_text.contains("LITELLM_API_KEY=sk-"), "{env_text}");
        // No legacy JSON is created.
        assert!(ops
            .file_bytes(&PathBuf::from("/home/test/.continue/config.json"))
            .is_none());

        // Second run: the YAML is merged in place — still exactly
        // one entry per gateway group, no sidecar.
        let out2 = step.run(&ctx).await.unwrap();
        assert_eq!(out2.state, StepState::Done);
        let canonical = ops
            .file_bytes(&PathBuf::from("/home/test/.continue/config.yaml"))
            .unwrap();
        let v: serde_yaml::Value =
            serde_yaml::from_str(&String::from_utf8(canonical).unwrap()).unwrap();
        assert_eq!(v["models"].as_sequence().unwrap().len(), 4);
        assert!(ops
            .file_bytes(&PathBuf::from(
                "/home/test/.continue/config.yaml.llm-manager"
            ))
            .is_none());
    }

    #[tokio::test]
    async fn vscode_install_merges_existing_continue_yaml_and_env() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_home(PathBuf::from("/home/test"));
        // Pete's own config.yaml: another model and top-level content.
        ops.with_file(
            PathBuf::from("/home/test/.continue/config.yaml"),
            b"name: My Config\nversion: 1.0.0\nschema: v1\nmodels:\n  - name: cloud (work)\n    provider: openai\n    model: gpt-5\n    apiBase: https://api.openai.com/v1\n    apiKey: literal\n".to_vec(),
        );
        ops.with_file(
            PathBuf::from("/home/test/.continue/.env"),
            b"# my secrets\nOTHER_KEY=keep-me\nLITELLM_API_KEY=sk-stale\n".to_vec(),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = VscodeExtensions::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let canonical = ops
            .file_bytes(&PathBuf::from("/home/test/.continue/config.yaml"))
            .unwrap();
        let v: serde_yaml::Value =
            serde_yaml::from_str(&String::from_utf8(canonical).unwrap()).unwrap();
        assert_eq!(v["name"].as_str().unwrap(), "My Config");
        let models = v["models"].as_sequence().unwrap();
        assert_eq!(models.len(), 5, "his model plus the four gateway models");
        assert!(models.iter().any(|m| m["model"].as_str() == Some("gpt-5")));
        assert!(models
            .iter()
            .any(|m| m["model"].as_str() == Some("planner")));
        // .env: our key rotated, his other variable preserved.
        let env = ops
            .file_bytes(&PathBuf::from("/home/test/.continue/.env"))
            .unwrap();
        let env_text = String::from_utf8(env).unwrap();
        assert!(env_text.contains("OTHER_KEY=keep-me"), "{env_text}");
        assert!(!env_text.contains("sk-stale"), "{env_text}");
        assert_eq!(env_text.matches("LITELLM_API_KEY=").count(), 1);
    }

    #[tokio::test]
    async fn vscode_install_retires_wizard_written_legacy_json() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_home(PathBuf::from("/home/test"));
        // A config.json in exactly the shape older wizard versions wrote.
        let legacy = serde_json::json!({"models": [{
            "title": "planner (local)", "provider": "openai", "model": "planner",
            "apiBase": "http://127.0.0.1:4000/v1", "apiKey": "sk-old"
        }]});
        ops.with_file(
            PathBuf::from("/home/test/.continue/config.json"),
            serde_json::to_vec(&legacy).unwrap(),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = VscodeExtensions::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(ops
            .file_bytes(&PathBuf::from("/home/test/.continue/config.json"))
            .is_none());
        assert!(ops
            .file_bytes(&PathBuf::from(
                "/home/test/.continue/config.json.llm-manager.bak"
            ))
            .is_some());
        assert!(ops
            .file_bytes(&PathBuf::from("/home/test/.continue/config.yaml"))
            .is_some());
    }

    #[test]
    fn env_merge_replaces_and_appends_by_key() {
        let merged =
            merge_env_contents("A=1\nLITELLM_API_KEY=old\nB=2\n", "LITELLM_API_KEY", "new");
        assert_eq!(merged, "A=1\nLITELLM_API_KEY=new\nB=2\n");
        let appended = merge_env_contents("A=1\n", "LITELLM_API_KEY", "new");
        assert_eq!(appended, "A=1\nLITELLM_API_KEY=new\n");
        let exported = merge_env_contents("export LITELLM_API_KEY=old\n", "LITELLM_API_KEY", "new");
        assert_eq!(exported, "LITELLM_API_KEY=new\n");
    }

    #[tokio::test]
    async fn vscode_install_uses_resolved_cli() {
        // The app process's PATH cannot see VS Code, but the ops
        // layer resolved the install (registry / install locations):
        // every command must use the resolved absolute path.
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_home(PathBuf::from("/home/test"));
        ops.with_vscode_cli("C:\\Program Files\\Microsoft VS Code\\bin\\code.cmd");
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = VscodeExtensions::default();
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let executed = ops.executed_commands();
        assert_eq!(executed.len(), 4);
        assert!(
            executed
                .iter()
                .all(|a| a[0] == "C:\\Program Files\\Microsoft VS Code\\bin\\code.cmd"),
            "{executed:?}"
        );
    }

    #[tokio::test]
    async fn vscode_install_skips_when_code_cli_missing() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_command_failure(
            &["code", "--version"],
            -1,
            "spawn failed: program not found",
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = VscodeExtensions::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Skipped, "{out:?}");
        assert!(out.message.contains("not installed"), "{}", out.message);
        // Only the probe ran — no install was attempted.
        assert_eq!(ops.executed_commands().len(), 1);
    }

    #[tokio::test]
    async fn vscode_audit_skips_when_code_cli_missing() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_command_failure(
            &["code", "--list-extensions"],
            -1,
            "spawn failed: program not found",
        );
        let ctx = ctx_with(ops, RunMode::Audit);
        let out = VscodeExtensions::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Skipped, "{out:?}");
    }

    #[tokio::test]
    async fn vscode_audit_checks_presence() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_listed_extensions(&[
            "continue.continue",
            "saoudrizwan.claude-dev",
            "vivswan.litellm-vscode-chat",
            "other",
        ]);
        let ctx = ctx_with(ops, RunMode::Audit);
        let out = VscodeExtensions::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done);

        let ops2 = Arc::new(FakeSystemOps::new());
        ops2.with_listed_extensions(&["continue.continue"]);
        let ctx2 = ctx_with(ops2, RunMode::Audit);
        let out2 = VscodeExtensions::default().run(&ctx2).await.unwrap();
        assert_eq!(out2.state, StepState::Failed);
        assert!(out2.message.contains("saoudrizwan.claude-dev"));
    }

    #[test]
    fn litellm_settings_merge_preserves_user_content_and_replaces_own_entry() {
        let existing = "{\n    // Pete's settings\n    \"editor.fontSize\": 14,\n    \"litellm-vscode-chat.servers\": [\n        { \"label\": \"Work\", \"baseUrl\": \"http://example:4000\" },\n        { \"label\": \"Local\", \"baseUrl\": \"http://127.0.0.1:4000\", \"auth\": { \"apiKey\": \"sk-stale\" } }\n    ]\n}\n";
        let entry = litellm_server_entry(4000, "sk-fresh");
        let merged = merge_litellm_server_setting(existing, &entry).unwrap();
        // The user's comment and unrelated setting survive.
        assert!(merged.contains("// Pete's settings"), "{merged}");
        assert!(merged.contains("\"editor.fontSize\": 14"), "{merged}");
        // Exactly one Local entry, carrying the fresh key; the other
        // server is untouched; no stale key remains.
        let v = jsonc_parser::parse_to_serde_value(&merged, &Default::default())
            .unwrap()
            .unwrap();
        let servers = v["litellm-vscode-chat.servers"].as_array().unwrap();
        assert_eq!(servers.len(), 2);
        let locals: Vec<_> = servers.iter().filter(|s| s["label"] == "Local").collect();
        assert_eq!(locals.len(), 1);
        assert_eq!(locals[0]["auth"]["apiKey"], "sk-fresh");
        assert!(servers.iter().any(|s| s["label"] == "Work"));
        assert!(!merged.contains("sk-stale"), "{merged}");
        // Merging again is a no-op (no duplicate append).
        let merged2 = merge_litellm_server_setting(&merged, &entry).unwrap();
        let v2 = jsonc_parser::parse_to_serde_value(&merged2, &Default::default())
            .unwrap()
            .unwrap();
        assert_eq!(
            v2["litellm-vscode-chat.servers"].as_array().unwrap().len(),
            2
        );
    }

    #[test]
    fn litellm_settings_merge_into_empty_settings() {
        let merged =
            merge_litellm_server_setting("{}", &litellm_server_entry(4000, "sk-k")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["litellm-vscode-chat.servers"][0]["label"], "Local");
        assert_eq!(
            v["litellm-vscode-chat.servers"][0]["baseUrl"],
            "http://127.0.0.1:4000"
        );
    }

    #[tokio::test]
    async fn vscode_install_configures_litellm_endpoint_and_agents() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_home(PathBuf::from("/home/test"));
        ops.with_credential_value(crate::LITELLM_MASTER_KEY_CRED_TARGET, "sk-test-master");
        // Pre-existing settings with an unrelated key.
        ops.with_file(
            PathBuf::from("/home/test/AppData/Roaming/Code/User/settings.json"),
            b"{\n    \"editor.fontSize\": 13\n}\n".to_vec(),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = VscodeExtensions::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.message.contains("LiteLLM endpoint configured"),
            "{}",
            out.message
        );
        assert!(
            out.message.contains("Copilot agent(s) written"),
            "{}",
            out.message
        );
        // The extension itself was installed.
        assert!(ops
            .executed_commands()
            .iter()
            .any(|a| a.iter().any(|x| x == "vivswan.litellm-vscode-chat")));
        // Settings: entry present with the master key, unrelated key kept.
        let settings = ops
            .file_bytes(&PathBuf::from(
                "/home/test/AppData/Roaming/Code/User/settings.json",
            ))
            .expect("settings written");
        let text = String::from_utf8(settings).unwrap();
        assert!(text.contains("\"editor.fontSize\": 13"), "{text}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let servers = v["litellm-vscode-chat.servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["baseUrl"], "http://127.0.0.1:4000");
        assert_eq!(servers[0]["auth"]["apiKey"], "sk-test-master");
        // Agent files: planner carries Pete's text; coder carries
        // edit/execute; every configured group got a file.
        let planner = ops
            .file_bytes(&PathBuf::from(
                "/home/test/.copilot/agents/litellm-planner.md",
            ))
            .expect("planner agent written");
        let planner_text = String::from_utf8(planner).unwrap();
        assert!(
            planner_text.contains("name: LiteLLM Planner"),
            "{planner_text}"
        );
        assert!(planner_text.contains("model: \"litellm/planner\""));
        assert!(planner_text.contains("tools: [read, search]"));
        assert!(planner_text.contains("software planning agent"));
        let coder = ops
            .file_bytes(&PathBuf::from(
                "/home/test/.copilot/agents/litellm-coder.md",
            ))
            .expect("coder agent written");
        let coder_text = String::from_utf8(coder).unwrap();
        assert!(coder_text.contains("tools: [read, search, edit, execute]"));
        for group in ["coder-fast", "tool-runner"] {
            assert!(
                ops.file_bytes(&PathBuf::from(format!(
                    "/home/test/.copilot/agents/litellm-{group}.md"
                )))
                .is_some(),
                "agent for {group}"
            );
        }
    }

    #[tokio::test]
    async fn vscode_audit_verifies_endpoint_and_agents_after_install() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_home(PathBuf::from("/home/test"));
        ops.with_listed_extensions(&[
            "continue.continue",
            "saoudrizwan.claude-dev",
            "vivswan.litellm-vscode-chat",
        ]);
        let install_ctx = ctx_with(ops.clone(), RunMode::Install);
        assert_eq!(
            VscodeExtensions::default()
                .run(&install_ctx)
                .await
                .unwrap()
                .state,
            StepState::Done
        );
        let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = VscodeExtensions::default().run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        // Remove one agent file: the audit must now fail and name it.
        crate::SystemOps::remove_file(
            ops.as_ref(),
            &PathBuf::from("/home/test/.copilot/agents/litellm-coder.md"),
        )
        .await
        .unwrap();
        let out = VscodeExtensions::default().run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(out.message.contains("coder"), "{}", out.message);
    }

    #[tokio::test]
    async fn smoke_test_fails_when_backend_never_healthy() {
        let ops = Arc::new(FakeSystemOps::new());
        script_backends_healthy(&ops);
        // The planner backend (first group member polled) stays in
        // its model-loading state.
        ops.with_http_get(
            "http://127.0.0.1:8081/health",
            crate::HttpResponse {
                status: 503,
                body: b"loading".to_vec(),
                elapsed_ms: 1,
            },
        );
        ops.with_http_post(
            "http://127.0.0.1:4000/v1/chat/completions",
            smoke_post_response(),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = SmokeTest {
            max_latency_ms: SMOKE_MAX_LATENCY_MS,
            ready_budget_ms: 40,
            ready_poll_ms: 5,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(
            out.message.contains("did not become healthy"),
            "{}",
            out.message
        );
        assert!(out.message.contains("planner"), "{}", out.message);
        // No gateway probe was attempted.
        assert!(ops.posts().is_empty());
    }

    // --- McpServers ---

    #[tokio::test]
    async fn mcp_servers_requires_credentials() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = ctx_with(ops, RunMode::Install);
        let out = McpServers.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        // Target name shown; values never shown (there are none to show).
        assert!(out.message.contains(GITHUB_CRED_TARGET), "{}", out.message);

        let ops2 = Arc::new(FakeSystemOps::new());
        ops2.with_credential(GITHUB_CRED_TARGET);
        ops2.with_credential(GDRIVE_CRED_TARGET);
        let ctx2 = ctx_with(ops2.clone(), RunMode::Install);
        let out2 = McpServers.run(&ctx2).await.unwrap();
        assert_eq!(out2.state, StepState::Done, "{out2:?}");
        let raw = ops2
            .file_bytes(&PathBuf::from("/data/mcp/mcp-servers.json"))
            .unwrap();
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("credential-manager:"), "{text}");
        assert!(text.contains(GITHUB_CRED_TARGET), "{text}");
    }

    // --- WinMlRegister ---

    fn winml_ctx(ops: Arc<FakeSystemOps>, mode: RunMode, model_id: Option<&str>) -> WizardContext {
        let mut config = test_config_with_models();
        config.winml.enabled = true;
        config.winml.model_id = model_id.map(str::to_string);
        WizardContext::new(config, PathBuf::from("/data"), mode, ops)
    }

    #[tokio::test]
    async fn winml_disabled_skips() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = ctx_with(ops, RunMode::Install); // winml disabled by default
        let out = WinMlRegister.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Skipped);
    }

    #[tokio::test]
    async fn winml_registers_small_model_with_limits() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_get(
            "http://127.0.0.1:8090/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx = winml_ctx(ops.clone(), RunMode::Install, Some("qwen3-8b"));
        let step = WinMlRegister;
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let raw = ops
            .file_bytes(&PathBuf::from("/data/winml/registration.json"))
            .expect("registration written");
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["model_id"], "qwen3-8b");
        assert_eq!(v["limits"]["supports_tensor_split"], false);
        assert_eq!(v["limits"]["experimental"], true);
    }

    #[tokio::test]
    async fn winml_rejects_large_model_class() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = winml_ctx(ops, RunMode::Install, Some("qwen3-32b"));
        let out = WinMlRegister.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("ineligible"), "{}", out.message);
    }

    #[tokio::test]
    async fn winml_fails_when_server_down() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = winml_ctx(ops, RunMode::Install, Some("qwen3-8b"));
        let out = WinMlRegister.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("not reachable"));
        assert!(WinMlRegister
            .remediation_hint()
            .unwrap()
            .contains("WinMLServer"));
    }

    // --- MxcPolicies ---

    #[tokio::test]
    async fn mxc_policy_written_validated_and_self_tested() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_command(
            &["wxc-exec.exe", "--probe"],
            FakeSystemOps::ok_output(r#"{"psec":"1.1","ingress_loopback":true}"#),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = MxcPolicies;
        assert_eq!(step.planned_commands(&ctx).len(), 2);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("Learning"), "{}", out.message);
        let raw = ops
            .file_bytes(&PathBuf::from("/data/mxc/policy.json"))
            .expect("policy written");
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["default_deny"], true);
        // Self-test ran the pinned policy in --dry-run mode (the
        // 1.0.0 CLI has no --policy/--self-test form).
        let executed = ops.executed_commands();
        assert!(executed.iter().any(|a| {
            a.contains(&"--config".to_string()) && a.contains(&"--dry-run".to_string())
        }));
    }

    #[tokio::test]
    async fn mxc_audit_revalidates_policy() {
        let ops = Arc::new(FakeSystemOps::new());
        let install_ctx = ctx_with(ops.clone(), RunMode::Install);
        assert_eq!(
            MxcPolicies.run(&install_ctx).await.unwrap().state,
            StepState::Done
        );
        let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = MxcPolicies.run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
    }

    #[tokio::test]
    async fn mxc_missing_runtime_skips() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_command_failure(
            &["wxc-exec.exe", "--probe"],
            -1,
            "spawn failed: program not found",
        );
        let ctx = ctx_with(ops, RunMode::Install);
        let out = MxcPolicies.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Skipped, "{out:?}");
        assert!(out.message.contains("not installed"), "{}", out.message);
    }

    #[tokio::test]
    async fn mxc_probe_failure_fails_step() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_command_failure(&["wxc-exec.exe", "--probe"], 1, "not found");
        let ctx = ctx_with(ops, RunMode::Install);
        let out = MxcPolicies.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("not available"), "{}", out.message);
    }

    // --- InstallMxc ---

    fn mxc_pin_for(tarball: &[u8], exec: &[u8], prep: &[u8]) -> MxcSdkPin {
        MxcSdkPin {
            version: "1.0.0".to_string(),
            tarball_asset: "mxc-sdk-1.0.0.tgz".to_string(),
            tarball_sha256: sha256_hex(tarball),
            tarball_size: tarball.len() as u64,
            wxc_exec_sha256: sha256_hex(exec),
            wxc_host_prep_sha256: sha256_hex(prep),
        }
    }

    #[tokio::test]
    async fn install_mxc_downloads_extracts_verifies_and_preps() {
        let ops = Arc::new(FakeSystemOps::new());
        let tarball = b"fake-tarball-bytes";
        let exec = b"fake-wxc-exec";
        let prep = b"fake-wxc-host-prep";
        let pin = mxc_pin_for(tarball, exec, prep);
        ops.with_download(&pin.tarball_url(), tarball.to_vec());
        ops.with_extracted_file(
            PathBuf::from("/data/mxc/bin/x64/wxc-exec.exe"),
            exec.to_vec(),
        );
        ops.with_extracted_file(
            PathBuf::from("/data/mxc/bin/x64/wxc-host-prep.exe"),
            prep.to_vec(),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallMxc::with_pin(pin.clone()).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.message.contains("host preparation complete"),
            "{}",
            out.message
        );
        assert_eq!(ops.downloaded_to().len(), 1);
        let extractions = ops.tar_gz_extractions();
        assert_eq!(extractions.len(), 1);
        assert_eq!(extractions[0].2, "package/bin/x64/");
        assert_eq!(extractions[0].3, "package/");
        let elevated = ops.elevated_calls();
        assert_eq!(elevated.len(), 1);
        assert_eq!(elevated[0][0], "cmd.exe");
        let marker = ops
            .file_bytes(&PathBuf::from("/data/mxc/.installed.json"))
            .expect("marker written");
        let v: serde_json::Value = serde_json::from_slice(&marker).unwrap();
        assert_eq!(v["host_prep"]["system_drive"], true);
        // A re-run is a no-op: no second download, no second prompt.
        let out2 = InstallMxc::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out2.state, StepState::Done, "{out2:?}");
        assert_eq!(ops.downloaded_to().len(), 1);
        assert_eq!(ops.elevated_calls().len(), 1);
    }

    #[tokio::test]
    async fn install_mxc_already_installed_skips_download() {
        let ops = Arc::new(FakeSystemOps::new());
        let exec = b"fake-wxc-exec";
        let prep = b"fake-wxc-host-prep";
        let pin = mxc_pin_for(b"unused-tarball", exec, prep);
        ops.with_file(
            PathBuf::from("/data/mxc/bin/x64/wxc-exec.exe"),
            exec.to_vec(),
        );
        ops.with_file(
            PathBuf::from("/data/mxc/bin/x64/wxc-host-prep.exe"),
            prep.to_vec(),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallMxc::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(ops.downloaded_to().is_empty());
        assert!(ops.tar_gz_extractions().is_empty());
        assert_eq!(ops.elevated_calls().len(), 1);
    }

    #[tokio::test]
    async fn install_mxc_tarball_checksum_mismatch_fails_closed() {
        let ops = Arc::new(FakeSystemOps::new());
        let pin = mxc_pin_for(b"the-real-tarball", b"e", b"p");
        ops.with_download(&pin.tarball_url(), b"tampered-bytes".to_vec());
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallMxc::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(out.message.contains("checksum mismatch"), "{}", out.message);
        assert!(ops.tar_gz_extractions().is_empty());
    }

    // --- InstallPostgres ---

    fn pg_pin_for(zip: &[u8]) -> PostgresPin {
        PostgresPin {
            version: "18.6-1".to_string(),
            asset: "postgresql-18.6-1-windows-x64-binaries.zip".to_string(),
            sha256: sha256_hex(zip),
            size: zip.len() as u64,
        }
    }

    fn stage_pg_binaries(ops: &FakeSystemOps) {
        for exe in [
            "pg_ctl.exe",
            "initdb.exe",
            "postgres.exe",
            "psql.exe",
            "createdb.exe",
        ] {
            ops.with_extracted_file(
                PathBuf::from("/data/postgres/bin").join(exe),
                b"fake-pg-binary".to_vec(),
            );
        }
    }

    #[tokio::test]
    async fn install_postgres_downloads_extracts_initializes_and_creates_db() {
        let ops = Arc::new(FakeSystemOps::new());
        let zip = b"fake-pg-zip";
        let pin = pg_pin_for(zip);
        ops.with_download(&pin.url(), zip.to_vec());
        stage_pg_binaries(&ops);
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallPostgres::with_pin(pin.clone())
            .run(&ctx)
            .await
            .unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.message.contains("running on 127.0.0.1:54329"),
            "{}",
            out.message
        );
        assert_eq!(ops.downloaded_to().len(), 1);
        let extractions = ops.zip_prefix_extractions();
        assert_eq!(extractions.len(), 1);
        assert_eq!(
            extractions[0].2,
            vec![
                "pgsql/bin/".to_string(),
                "pgsql/lib/".to_string(),
                "pgsql/share/".to_string(),
                "pgsql/include/".to_string()
            ]
        );
        assert_eq!(extractions[0].3, "pgsql/");
        // initdb ran with scram auth; createdb made the litellm db.
        let executed = ops.executed_commands();
        let initdb = executed
            .iter()
            .find(|a| a.iter().any(|x| x.ends_with("initdb.exe")))
            .expect("initdb ran");
        assert!(initdb.iter().any(|x| x == "scram-sha-256"), "{initdb:?}");
        let createdb = executed
            .iter()
            .find(|a| a.iter().any(|x| x.ends_with("createdb.exe")))
            .expect("createdb ran");
        assert_eq!(createdb.last().map(String::as_str), Some("litellm"));
        // The password file never survives the step.
        assert!(ops
            .file_bytes(&PathBuf::from("/data/postgres/pwfile.tmp"))
            .is_none());
        // The generated password is stored for the gateway spawns.
        let stored = ops
            .credential_value(crate::POSTGRES_PASSWORD_CRED_TARGET)
            .expect("postgres password stored");
        assert_eq!(stored.len(), 64, "hex password: {stored}");
        let marker = ops
            .file_bytes(&PathBuf::from("/data/postgres/.installed.json"))
            .expect("marker written");
        let v: serde_json::Value = serde_json::from_slice(&marker).unwrap();
        assert_eq!(v["database"], "litellm");
        assert_eq!(v["port"], 54329);
    }

    #[tokio::test]
    async fn install_postgres_skips_extraction_without_marker_when_cluster_present() {
        // Pete's machine, 2026-10-16: a working install whose
        // .installed.json marker was lost. Binaries + initialized
        // cluster are the ground truth — the step must not
        // re-extract over the running server's DLLs; it rewrites
        // the marker and completes.
        let ops = Arc::new(FakeSystemOps::new());
        let pin = pg_pin_for(b"unused");
        for exe in [
            "pg_ctl.exe",
            "initdb.exe",
            "postgres.exe",
            "psql.exe",
            "createdb.exe",
        ] {
            ops.with_file(PathBuf::from("/data/postgres/bin").join(exe), b"x".to_vec());
        }
        ops.with_file(
            PathBuf::from("/data/postgres/data/PG_VERSION"),
            b"18\n".to_vec(),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallPostgres::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(ops.zip_prefix_extractions().is_empty());
        assert!(ops.downloaded_to().is_empty());
        assert!(
            ops.file_bytes(&PathBuf::from("/data/postgres/.installed.json"))
                .is_some(),
            "marker rewritten"
        );
    }

    #[tokio::test]
    async fn install_postgres_stops_running_server_before_extract() {
        // Re-extraction IS required (no cluster yet), and the fake
        // reports the server ready (its run_command succeeds for
        // pg_isready): the step must issue pg_ctl stop before
        // extracting, or the real extraction dies on locked DLLs.
        let ops = Arc::new(FakeSystemOps::new());
        let zip = b"fake-pg-zip";
        let pin = pg_pin_for(zip);
        ops.with_download(&pin.url(), zip.to_vec());
        stage_pg_binaries(&ops);
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallPostgres::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert_eq!(ops.zip_prefix_extractions().len(), 1);
        let executed = ops.executed_commands();
        let stop = executed
            .iter()
            .find(|a| a.iter().any(|x| x.ends_with("pg_ctl.exe")) && a.iter().any(|x| x == "stop"))
            .expect("pg_ctl stop issued before extraction");
        assert!(stop.iter().any(|x| x == "fast"), "{stop:?}");
    }

    #[tokio::test]
    async fn install_postgres_checksum_mismatch_fails_closed() {
        let ops = Arc::new(FakeSystemOps::new());
        let pin = pg_pin_for(b"the-real-zip");
        ops.with_download(&pin.url(), b"tampered-bytes".to_vec());
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallPostgres::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(out.message.contains("checksum mismatch"), "{}", out.message);
        assert!(ops.zip_prefix_extractions().is_empty());
    }

    #[tokio::test]
    async fn install_postgres_audit_requires_install_and_cluster() {
        let ops = Arc::new(FakeSystemOps::new());
        let pin = pg_pin_for(b"unused");
        let ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = InstallPostgres::with_pin(pin.clone())
            .run(&ctx)
            .await
            .unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(out.message.contains("not installed"), "{}", out.message);
        // Fully staged install passes the audit without running
        // anything.
        for exe in [
            "pg_ctl.exe",
            "initdb.exe",
            "postgres.exe",
            "psql.exe",
            "createdb.exe",
        ] {
            ops.with_file(PathBuf::from("/data/postgres/bin").join(exe), b"x".to_vec());
        }
        ops.with_file(
            PathBuf::from("/data/postgres/.installed.json"),
            b"{}".to_vec(),
        );
        ops.with_file(
            PathBuf::from("/data/postgres/data/PG_VERSION"),
            b"18\n".to_vec(),
        );
        let out = InstallPostgres::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(ops.executed_commands().is_empty());
    }

    #[tokio::test]
    async fn start_gateway_passes_database_url_when_postgres_installed() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_file(
            PathBuf::from("/data/postgres/.installed.json"),
            b"{}".to_vec(),
        );
        ops.with_file(
            PathBuf::from("/data/postgres/bin/pg_ctl.exe"),
            b"fake".to_vec(),
        );
        ops.with_credential_value(crate::POSTGRES_PASSWORD_CRED_TARGET, "deadbeefcafe");
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let envs = ops.spawn_envs();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].get("DATABASE_URL").map(String::as_str),
            Some("postgresql://postgres:deadbeefcafe@127.0.0.1:54329/litellm")
        );
        // Without the install, the gateway spawns database-free.
        let ops2 = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops2);
        ops2.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx2 = ctx_with(ops2.clone(), RunMode::Install);
        let out2 = step.run(&ctx2).await.unwrap();
        assert_eq!(out2.state, StepState::Done, "{out2:?}");
        assert!(!ops2.spawn_envs()[0].contains_key("DATABASE_URL"));
    }

    #[tokio::test]
    async fn install_mxc_declined_elevation_completes_with_prep_outstanding() {
        let ops = Arc::new(FakeSystemOps::new());
        let exec = b"fake-wxc-exec";
        let prep = b"fake-wxc-host-prep";
        let pin = mxc_pin_for(b"unused-tarball", exec, prep);
        ops.with_file(
            PathBuf::from("/data/mxc/bin/x64/wxc-exec.exe"),
            exec.to_vec(),
        );
        ops.with_file(
            PathBuf::from("/data/mxc/bin/x64/wxc-host-prep.exe"),
            prep.to_vec(),
        );
        ops.with_elevated_error("elevation failed or was declined (ShellExecuteEx error 1223)");
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = InstallMxc::with_pin(pin).run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.message.contains("host preparation was not completed"),
            "{}",
            out.message
        );
        let marker = ops
            .file_bytes(&PathBuf::from("/data/mxc/.installed.json"))
            .expect("marker written");
        let v: serde_json::Value = serde_json::from_slice(&marker).unwrap();
        assert_eq!(v["host_prep"]["system_drive"], false);
    }

    #[tokio::test]
    async fn install_mxc_audit_requires_install_then_verifies() {
        let ops = Arc::new(FakeSystemOps::new());
        let exec = b"fake-wxc-exec";
        let prep = b"fake-wxc-host-prep";
        let pin = mxc_pin_for(b"unused-tarball", exec, prep);
        let audit_ctx = ctx_with(ops.clone(), RunMode::Audit);
        let out = InstallMxc::with_pin(pin.clone())
            .run(&audit_ctx)
            .await
            .unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(out.message.contains("not installed"), "{}", out.message);
        ops.with_file(
            PathBuf::from("/data/mxc/bin/x64/wxc-exec.exe"),
            exec.to_vec(),
        );
        ops.with_file(
            PathBuf::from("/data/mxc/bin/x64/wxc-host-prep.exe"),
            prep.to_vec(),
        );
        let install_ctx = ctx_with(ops.clone(), RunMode::Install);
        assert_eq!(
            InstallMxc::with_pin(pin.clone())
                .run(&install_ctx)
                .await
                .unwrap()
                .state,
            StepState::Done
        );
        let out = InstallMxc::with_pin(pin).run(&audit_ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
    }

    #[tokio::test]
    async fn install_mxc_unconfigured_fails_closed() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = ctx_with(ops, RunMode::Install);
        let out = InstallMxc::unconfigured().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("no MXC SDK build pinned"));
    }

    // --- SmokeTest ---

    /// Install-mode smoke tests: the step waits for every enabled
    /// member backend to answer /health before probing. Script all
    /// four default backends healthy.
    fn script_backends_healthy(ops: &FakeSystemOps) {
        for port in [8081u16, 8082, 8083, 8084] {
            ops.with_http_get(
                &format!("http://127.0.0.1:{port}/health"),
                FakeSystemOps::http_ok("{}"),
            );
        }
    }

    #[tokio::test]
    async fn smoke_test_passes_and_writes_baselines() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_post(
            "http://127.0.0.1:4000/v1/chat/completions",
            smoke_post_response(),
        );
        script_backends_healthy(&ops);
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = SmokeTest::default();
        assert_eq!(step.planned_commands(&ctx).len(), 4, "one probe per group");
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("4 group(s)"));
        assert_eq!(ops.posts().len(), 4);
        // Tag present in every request body.
        for (_, body) in ops.posts() {
            assert!(body.contains("wizard smoke test wizard-"), "{body}");
        }
        let raw = ops
            .file_bytes(&PathBuf::from("/data/wizard/smoke-baselines.json"))
            .expect("baselines written");
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["baselines"].as_array().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn smoke_test_fails_on_empty_completion() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_post(
            "http://127.0.0.1:4000/v1/chat/completions",
            FakeSystemOps::http_ok(r#"{"choices":[{"message":{"content":"  "}}]}"#),
        );
        script_backends_healthy(&ops);
        let ctx = ctx_with(ops, RunMode::Install);
        let out = SmokeTest::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("empty completion"));
    }

    #[tokio::test]
    async fn smoke_test_passes_on_reasoning_only_completion() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_post(
            "http://127.0.0.1:4000/v1/chat/completions",
            FakeSystemOps::http_ok(
                r#"{"choices":[{"message":{"content":"","reasoning_content":"thinking about the probe"},"finish_reason":"length"}]}"#,
            ),
        );
        script_backends_healthy(&ops);
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = SmokeTest::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.log_tail.iter().any(|l| l.contains("reasoning-only")),
            "{out:?}"
        );
        // The probe asks the backend not to think and gives the
        // reply room to arrive.
        let posts = ops.posts();
        assert!(posts[0].1.contains("enable_thinking"), "{}", posts[0].1);
        assert!(posts[0].1.contains("\"max_tokens\":128"), "{}", posts[0].1);
    }

    #[tokio::test]
    async fn smoke_test_fails_on_http_error_status() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_http_post(
            "http://127.0.0.1:4000/v1/chat/completions",
            crate::HttpResponse {
                status: 500,
                body: b"boom".to_vec(),
                elapsed_ms: 5,
            },
        );
        script_backends_healthy(&ops);
        let ctx = ctx_with(ops, RunMode::Install);
        let out = SmokeTest::default().run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("500"));
    }

    // --- Step registry invariants ---

    #[test]
    fn default_steps_have_unique_ids_and_valid_prereq_order() {
        let steps = default_steps();
        assert_eq!(steps.len(), 12);
        let ids: Vec<&str> = steps.iter().map(|s| s.id()).collect();
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            assert!(seen.insert(*id), "duplicate id {id}");
        }
        // Every prerequisite names an earlier step (topological order).
        for (i, s) in steps.iter().enumerate() {
            for pre in s.prerequisites() {
                let pos = ids.iter().position(|id| *id == pre.as_str());
                assert!(
                    pos.map(|p| p < i).unwrap_or(false),
                    "step {} has bad prerequisite {pre}",
                    s.id()
                );
            }
        }
        // All eleven are checks (audit covers everything).
        assert!(steps.iter().all(|s| s.is_check()));
    }

    #[test]
    fn every_step_has_a_remediation_hint() {
        for s in default_steps() {
            assert!(
                s.remediation_hint().is_some(),
                "step {} lacks a remediation hint",
                s.id()
            );
        }
    }

    #[test]
    fn planned_commands_match_mode() {
        let _guard = PathGuard::set_to(&path_dir_with_uv(true));
        // Install mode: StartGateway shows the five commands it may
        // spawn (two venv probes, venv creation, pip install, spawn)...
        let install_ctx = WizardContext::new(
            test_config_with_models(),
            PathBuf::from("/data"),
            RunMode::Install,
            Arc::new(FakeSystemOps::new()),
        );
        let install_cmds = StartGateway::default().planned_commands(&install_ctx);
        assert_eq!(install_cmds.len(), 7);
        assert!(install_cmds[2].argv[0].ends_with("uv"));

        // ...audit mode shows only the health probe.
        let audit_ctx = WizardContext::new(
            test_config_with_models(),
            PathBuf::from("/data"),
            RunMode::Audit,
            Arc::new(FakeSystemOps::new()),
        );
        let audit_cmds = StartGateway::default().planned_commands(&audit_ctx);
        assert_eq!(audit_cmds.len(), 1);
        assert_eq!(audit_cmds[0].argv[0], "http-get");

        // VscodeExtensions: install commands vs list-extensions probe.
        let install_cmds = VscodeExtensions::default().planned_commands(&install_ctx);
        assert!(install_cmds.iter().all(|c| c.argv[0] == "code"));
        assert!(install_cmds
            .iter()
            .all(|c| c.argv.contains(&"--install-extension".to_string())));
        let audit_cmds = VscodeExtensions::default().planned_commands(&audit_ctx);
        assert_eq!(audit_cmds.len(), 1);
        assert_eq!(
            audit_cmds[0].argv,
            vec!["code".to_string(), "--list-extensions".to_string()]
        );

        // DetectHardware: the per-GPU query plus the driver-version header
        // probe that RealSystemOps::gpu_inventory actually runs. argv[0]
        // is telemetry's resolved smi path (bare "nvidia-smi" unless a
        // System32 copy exists).
        let smi = manager_telemetry::default_smi_path()
            .to_string_lossy()
            .into_owned();
        let hw_cmds = DetectHardware.planned_commands(&install_ctx);
        assert_eq!(hw_cmds.len(), 2);
        assert_eq!(hw_cmds[0].argv[0], smi);
        assert!(hw_cmds[0].argv.iter().any(|a| a.contains("--query-gpu")));
        assert_eq!(hw_cmds[1].argv, vec![smi]);
    }
}

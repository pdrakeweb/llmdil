//! The ten concrete wizard steps (spec §4.1).
//!
//! Steps branch on [`RunMode`]: in `Install` they perform the install; in
//! `Audit` they re-verify without reinstalling. All system effects go through
//! [`SystemOps`](crate::SystemOps), so every step is hermetic under
//! [`FakeSystemOps`](crate::fake::FakeSystemOps).

use crate::{
    checksums_match, is_pinned_hash, CommandOutput, PlannedCommand, RunMode, StepLogSink,
    StepOutcome, SystemOps, WizardContext, WizardError, WizardStep,
};
use async_trait::async_trait;
use manager_config::{AppConfig, BackendConfig, McpExposure, ModelConfig, SplitMode, SupervisionMode};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Step progress logging
// ---------------------------------------------------------------------------
//
// A step's collected log (the `Vec<String>` every step builds) is
// persisted by the caller only when the step RETURNS, so on its own
// it leaves the wizard log silent for the whole run — a cold
// multi-minute install reads as a hang (field report 2026-10-19).
// The helpers here pair the collected log with the context's live
// sink: phase boundaries and verdicts go to both ([`push_log`]);
// high-frequency progress (download percentages, wait ticks) goes
// to the sink only ([`WizardContext::emit_log`]), so the collected
// log stays a readable end-of-step record instead of a transcript
// of every tick.

/// Record one step log line: appended to the step's collected log
/// and emitted to the live sink at the same moment.
fn push_log(ctx: &WizardContext, log: &mut Vec<String>, line: String) {
    ctx.emit_log(&line);
    log.push(line);
}

/// Compact elapsed label for start/finish pairs ("83.2s", "2m 05s").
fn fmt_elapsed(start: std::time::Instant) -> String {
    let secs = start.elapsed().as_secs_f64();
    if secs < 120.0 {
        format!("{secs:.1}s")
    } else {
        let total = secs.round() as u64;
        format!("{}m {:02}s", total / 60, total % 60)
    }
}

/// The last `max_lines` lines of a captured output, for attaching a
/// failing command's own words to the step log without dumping the
/// whole capture.
fn output_tail_lines(text: &str, max_lines: usize) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(max_lines)..].to_vec()
}

/// One-line failure evidence for a probe: exit code plus the tail
/// of whichever capture carries text (a Python probe's traceback
/// lands on stderr; a version print lands on stdout). Probes used
/// to discard this and report a bare bool/None, which threw away
/// exactly the line that says WHY a healthy-looking install failed
/// verification — the failure then looked identical to a missing
/// install and triggered the full repair path blind.
fn probe_failure_detail(out: &CommandOutput) -> String {
    let capture = if !out.stderr.trim().is_empty() {
        &out.stderr
    } else {
        &out.stdout
    };
    let tail = output_tail_lines(capture.trim(), 3).join(" | ");
    if tail.is_empty() {
        format!("exit {} (no output)", out.exit_code)
    } else {
        format!("exit {}: {tail}", out.exit_code)
    }
}

/// Extract a `major.minor` version from a probe's stdout: the LAST
/// line that is exactly a two-part version. Whole-stdout equality
/// (the previous parse) treated any extra output — an interpreter
/// warning, a sitecustomize banner — as a different version, which
/// read as "wrong Python" and forced a full venv rebuild on every
/// start. Noise around the version is not a different version.
fn parse_two_part_version(stdout: &str) -> Option<String> {
    stdout.lines().rev().find_map(|line| {
        let line = line.trim();
        let mut parts = line.split('.');
        let major = parts.next()?;
        let minor = parts.next()?;
        if parts.next().is_some()
            || major.is_empty()
            || minor.is_empty()
            || !major.bytes().all(|b| b.is_ascii_digit())
            || !minor.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        Some(line.to_string())
    })
}

/// Extract a `major.minor.patch` version from a probe's stdout:
/// the LAST line that starts with three numeric components (a
/// pre-release/build suffix is allowed). See
/// [`parse_two_part_version`] for why the parse tolerates noise.
fn parse_semver_version(stdout: &str) -> Option<String> {
    stdout.lines().rev().find_map(|line| {
        let line = line.trim();
        let mut numeric = 0usize;
        let mut end = 0usize;
        for (i, c) in line.char_indices() {
            if c.is_ascii_digit() {
                end = i + c.len_utf8();
            } else if c == '.' && numeric < 3 {
                numeric += 1;
                end = i + 1;
            } else {
                break;
            }
        }
        // Count the components in the consumed digit/dot prefix.
        let prefix = &line[..end];
        let components = prefix.split('.').filter(|p| !p.is_empty()).count();
        if components == 3 && prefix.bytes().filter(|&b| b == b'.').count() == 2 {
            // The character after the patch digits must not extend
            // the number (a fourth component is a different shape).
            match line[end..].chars().next() {
                None => Some(line.to_string()),
                Some(c) if !c.is_ascii_digit() && c != '.' => Some(line.to_string()),
                _ => None,
            }
        } else {
            None
        }
    })
}

/// Run a command with the step log wrapped around it: the argv is
/// recorded BEFORE the run (so a long command is never a silent
/// window), the exit and elapsed time after it, and on a non-zero
/// exit the tail of the captured output joins the log — a failed
/// command's own words belong in the step log, not only in the
/// failure message. A spawn/timeout error is logged the same way
/// and returned to the caller.
async fn run_command_logged(
    ctx: &WizardContext,
    log: &mut Vec<String>,
    cmd: &PlannedCommand,
    timeout: Option<std::time::Duration>,
) -> Result<CommandOutput, WizardError> {
    push_log(ctx, log, format!("running: {}", cmd.argv.join(" ")));
    let started = std::time::Instant::now();
    let result = match timeout {
        Some(budget) => ctx.ops.run_command_with_timeout(cmd, budget).await,
        None => ctx.ops.run_command(cmd).await,
    };
    match result {
        Ok(out) => {
            push_log(
                ctx,
                log,
                format!("exit {} in {}", out.exit_code, fmt_elapsed(started)),
            );
            if !out.success() {
                let (label, capture) = if !out.stderr.trim().is_empty() {
                    ("stderr", &out.stderr)
                } else {
                    ("stdout", &out.stdout)
                };
                let tail = output_tail_lines(capture.trim(), 10);
                if !tail.is_empty() {
                    push_log(ctx, log, format!("command {label} (last lines):"));
                    for line in tail {
                        push_log(ctx, log, line.to_string());
                    }
                }
            }
            Ok(out)
        }
        Err(e) => {
            push_log(
                ctx,
                log,
                format!("command failed in {}: {e}", fmt_elapsed(started)),
            );
            Err(e)
        }
    }
}

/// Throttled download progress for the live sink: the first call,
/// every ~10-point percentage step, at least every 5 s while bytes
/// move, and completion. The collected step log keeps only the
/// download's start/finish lines (recorded by the caller) — the
/// percentage ticks are live-only progress, not record.
struct ProgressEmitter {
    sink: Option<StepLogSink>,
    label: String,
    started: std::time::Instant,
    last_emit_ms: std::sync::atomic::AtomicU64,
    last_pct: std::sync::atomic::AtomicU64,
}

impl ProgressEmitter {
    fn new(ctx: &WizardContext, label: impl Into<String>) -> Self {
        Self {
            sink: ctx.log_sink(),
            label: label.into(),
            started: std::time::Instant::now(),
            last_emit_ms: std::sync::atomic::AtomicU64::new(u64::MAX),
            // u64::MAX = no percentage emitted yet.
            last_pct: std::sync::atomic::AtomicU64::new(u64::MAX),
        }
    }

    fn progress(&self, downloaded: u64, total: u64) {
        use std::sync::atomic::Ordering;
        let Some(sink) = &self.sink else {
            return;
        };
        let pct = downloaded.saturating_mul(100).checked_div(total);
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        let last_ms = self.last_emit_ms.load(Ordering::Relaxed);
        let first = last_ms == u64::MAX;
        let pct_step = match (pct, self.last_pct.load(Ordering::Relaxed)) {
            (Some(p), prev) => prev == u64::MAX || p >= prev + 10,
            (None, _) => false,
        };
        let complete = total > 0 && downloaded >= total;
        let due = first || pct_step || complete || elapsed_ms >= last_ms + 5_000;
        if !due {
            return;
        }
        self.last_emit_ms.store(elapsed_ms, Ordering::Relaxed);
        if let Some(p) = pct {
            self.last_pct.store(p, Ordering::Relaxed);
        }
        sink(match pct {
            Some(p) => format!(
                "downloading {}: {downloaded} / {total} bytes ({p}%)",
                self.label
            ),
            None => format!("downloading {}: {downloaded} bytes", self.label),
        });
    }
}

/// Incremental reader over an append-only log file (the
/// postmaster's own log): each call returns the lines written since
/// the previous call, so a wait loop can stream a starting
/// server's FATAL lines into the step log as they land instead of
/// only attaching a tail after the wait has already failed.
struct FileLineStreamer {
    emitted: usize,
}

impl FileLineStreamer {
    fn new() -> Self {
        Self { emitted: 0 }
    }

    /// Start from the file's current end: lines already in the
    /// file predate this wait and are not this wait's news.
    async fn prime(&mut self, ops: &dyn SystemOps, path: &Path) {
        self.emitted = match ops.read_file(path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).lines().count(),
            Err(_) => 0,
        };
    }

    /// Emit every line appended since the last call, prefixed so
    /// the step log attributes them to their writer.
    async fn stream_new(
        &mut self,
        ctx: &WizardContext,
        ops: &dyn SystemOps,
        path: &Path,
        prefix: &str,
        log: &mut Vec<String>,
    ) {
        let Ok(bytes) = ops.read_file(path).await else {
            return;
        };
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() < self.emitted {
            // The file shrank (rotated/recreated): resync to its
            // end rather than replaying it.
            self.emitted = lines.len();
            return;
        }
        for line in &lines[self.emitted..] {
            push_log(ctx, log, format!("{prefix}{line}"));
        }
        self.emitted = lines.len();
    }
}
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
                        push_log(ctx, &mut log, format!("downloading {url}"));
                        // The planned commands must equal the spawned
                        // actions: the downloads in this loop are exactly
                        // planned_commands()[0] and planned_commands()[1],
                        // in this order, when they are needed at all.
                        let progress = ProgressEmitter::new(ctx, archive_pin.asset.clone());
                        let dl_started = std::time::Instant::now();
                        match ctx
                            .ops
                            .download(&url, &archive, &|d, t| progress.progress(d, t))
                            .await
                        {
                            Ok(bytes) => push_log(
                                ctx,
                                &mut log,
                                format!("downloaded {bytes} bytes in {}", fmt_elapsed(dl_started)),
                            ),
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
                    push_log(
                        ctx,
                        &mut log,
                        format!("extracting {} to {}", archive_pin.asset, dir.display()),
                    );
                    let extract_started = std::time::Instant::now();
                    if let Err(e) = ctx.ops.extract_zip(&archive, &dir).await {
                        return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
                    }
                    push_log(
                        ctx,
                        &mut log,
                        format!(
                            "extracted {} to {} in {}",
                            archive_pin.asset,
                            dir.display(),
                            fmt_elapsed(extract_started)
                        ),
                    );
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
    fn model_for<'a>(
        &self,
        ctx: &'a WizardContext,
        backend: &BackendConfig,
    ) -> Option<&'a manager_config::ModelConfig> {
        let target = resolve_dest(&ctx.data_dir, &backend.model_file);
        ctx.config
            .models
            .iter()
            .find(|m| resolve_dest(&ctx.data_dir, &m.gguf_path) == target)
    }

    async fn model_bytes(&self, ctx: &WizardContext, backend: &BackendConfig) -> u64 {
        match self.model_for(ctx, backend) {
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

    /// The share of a backend's model that actually occupies GPU
    /// memory under expert offload, from the curated catalog's
    /// pinned expert total (the step runs before downloads, so the
    /// GGUF itself is usually unavailable; an unpinned split falls
    /// back to the full size). Non-offload backends return `total`
    /// unchanged.
    fn gpu_resident_bytes(&self, ctx: &WizardContext, backend: &BackendConfig, total: u64) -> u64 {
        if manager_models::expert_offload(backend.raw_flags.as_deref())
            != manager_models::ExpertOffload::AllExperts
        {
            return total;
        }
        let Some(model) = self.model_for(ctx, backend) else {
            return total;
        };
        let pin = manager_models::curated_models()
            .into_iter()
            .find(|s| s.id == model.id)
            .and_then(|s| s.expert_bytes);
        match pin {
            Some(experts) => total.saturating_sub(experts),
            None => total,
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
            let total = self.model_bytes(ctx, b).await;
            // A backend served by expert offload is not split: its
            // entry only accounts for the GPU-resident share of its
            // weights on GPU 0, where llama.cpp places an unsplit
            // backend.
            let offload = manager_models::expert_offload(b.raw_flags.as_deref())
                != manager_models::ExpertOffload::None;
            inputs.push(SplitInput {
                model_bytes: if offload {
                    self.gpu_resident_bytes(ctx, b, total)
                } else {
                    total
                },
                existing: if b.flags.tensor_split.is_empty() {
                    None
                } else {
                    Some(b.flags.tensor_split.clone())
                },
                assign: !offload,
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

    fn run_audit(&self, ctx: &WizardContext, vrams: &[u64]) -> StepOutcome {
        let gpu_count = vrams.len();
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
                // An empty split is a valid placement when the
                // backend needs no split: expert offload serves it
                // unsplit by design, and a model whose GPU-resident
                // weights fit GPU 0 is placed whole by llama.cpp.
                if manager_models::expert_offload(b.raw_flags.as_deref())
                    != manager_models::ExpertOffload::None
                {
                    log.push(format!(
                        "{}: no split — served by expert offload, experts in system RAM",
                        b.id
                    ));
                    continue;
                }
                let total = match self.model_for(ctx, b) {
                    Some(m) => manager_models::curated_models()
                        .into_iter()
                        .find(|s| s.id == m.id)
                        .map(|s| s.size_bytes)
                        .unwrap_or_else(|| estimate_model_bytes(m.params_b as f64)),
                    None => 0,
                };
                let resident = self.gpu_resident_bytes(ctx, b, total);
                // An unsplit backend launches on the DEFAULT
                // device, GPU 0 — not whichever GPU is largest.
                // Crediting the largest GPU's capacity cleared
                // models that then failed to start on a smaller
                // GPU 0 (code-review 2.14).
                let gpu0 = vrams.first().copied().unwrap_or(0) * manager_models::MIB;
                if resident > 0 && resident <= gpu0 {
                    log.push(format!(
                        "{}: no split — GPU-resident weights fit GPU 0",
                        b.id
                    ));
                    continue;
                }
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
            RunMode::Audit => Ok(self.run_audit(ctx, &vrams)),
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
                        push_log(
                            ctx,
                            &mut log,
                            format!("downloading {} → {}", url, dest.display()),
                        );
                        let progress = ProgressEmitter::new(ctx, model.id.clone());
                        if let Err(e) = ctx
                            .ops
                            .download(&url, &dest, &|d, t| progress.progress(d, t))
                            .await
                        {
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
                        Self::stamp_verified_at(ctx, &model.id, &model.sha256, &mut log);
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
                    // Verified-by-stamp shortcut: `verified_at` is
                    // stamped only after a hash match (a download
                    // verification or an earlier audit) — but the
                    // stamp alone does not say WHAT was verified.
                    // The skip therefore also requires the stamp's
                    // recorded hash (`verified_sha256`) to equal the
                    // CURRENT pin and the file to still have exactly
                    // the catalog size; then the model is not
                    // re-hashed. Without this, the launch-time audit
                    // re-reads every multi-GiB GGUF on every start
                    // for no new information — but with only the
                    // timestamp and the length checked, a
                    // same-length replacement file, or a catalog
                    // re-pin, inherited trust it never earned
                    // (code-review 2.4). Any doubt — no stamp, a
                    // stamp from before hash binding existed, a
                    // stamp against a different pin, no catalog
                    // entry to pin the size, a failed stat, a length
                    // mismatch — falls through to the full hash
                    // below.
                    if model.verified_at.is_some()
                        && model
                            .verified_sha256
                            .as_deref()
                            .is_some_and(|s| checksums_match(s, &model.sha256))
                    {
                        let expected = manager_models::curated_models()
                            .iter()
                            .find(|s| s.id == model.id)
                            .map(|s| s.size_bytes);
                        if let (Some(expected), Some(actual)) =
                            (expected, ctx.ops.file_len(&dest).await)
                        {
                            if actual == expected {
                                log.push(format!(
                                    "{}: verified by an earlier checksum pass against the current pin (stamp + file length match; not re-hashed this run)",
                                    model.id
                                ));
                                verified += 1;
                                continue;
                            }
                        }
                    }
                    match ctx.ops.sha256_file(&dest).await {
                        Ok(h) if verify(&h) => {
                            log.push(format!("{}: checksum verified", model.id));
                            Self::stamp_verified_at(ctx, &model.id, &model.sha256, &mut log);
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
    fn stamp_verified_at(
        ctx: &WizardContext,
        model_id: &str,
        verified_sha256: &str,
        log: &mut Vec<String>,
    ) {
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
        // The stamp records WHICH pinned hash the file was verified
        // against; the audit's verified-by-stamp shortcut trusts a
        // stamp only while that still equals the current pin.
        model.verified_sha256 = Some(verified_sha256.to_string());
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

/// Transform a fully built gateway spawn command into the service
/// launch spec for its definition. The argv and non-secret env carry
/// over verbatim; the secret-bearing values become credential
/// references the service host resolves at start, so a secret value
/// is never written into the definition file: the master key and the
/// OpenRouter key by their Credential Manager targets, `DATABASE_URL`
/// as the composable Postgres form (password credential + port), and
/// every armed gateway MCP server's credential under each of its
/// env-key names by that server's `cred_ref` target (code-review
/// B-7). The MCP mapping is safe for arbitrary env names because
/// the spawn env only carries an MCP env key when the arming pass
/// resolved that server's credential into it, and the service host
/// resolves any `CredentialRef` entry generically by target
/// (`manager_services::resolve_env` is not name-locked).
fn gateway_launch_spec(
    cmd: &PlannedCommand,
    config: &AppConfig,
) -> manager_services::LaunchSpec {
    // Env-key name -> Credential Manager target, for every
    // gateway-exposed MCP server that names a credential. The first
    // server wins on a shared env-key name (the spawn env itself
    // holds only one value per name, from the same iteration).
    let mut mcp_cred_refs: std::collections::HashMap<&str, &str> =
        std::collections::HashMap::new();
    for server in &config.mcp.servers {
        if server.exposure != manager_config::McpExposure::Gateway {
            continue;
        }
        let Some(target) = server.cred_ref.as_deref() else {
            continue;
        };
        for key in &server.env_keys {
            mcp_cred_refs.entry(key.as_str()).or_insert(target);
        }
    }
    let mut env: Vec<manager_services::EnvEntry> = cmd
        .env
        .iter()
        .map(|(name, value)| match name.as_str() {
            "LITELLM_MASTER_KEY" => manager_services::EnvEntry::credential_ref(
                name,
                crate::LITELLM_MASTER_KEY_CRED_TARGET,
            ),
            "DATABASE_URL" => manager_services::EnvEntry {
                name: name.clone(),
                value: manager_services::EnvValue::DatabaseUrl {
                    password_credential: crate::POSTGRES_PASSWORD_CRED_TARGET.to_string(),
                    port: config.postgres.port,
                },
            },
            "OPENROUTER_API_KEY" => match &config.gateway.openrouter.cred_ref {
                Some(target) => manager_services::EnvEntry::credential_ref(name, target),
                None => manager_services::EnvEntry::literal(name, value.clone()),
            },
            _ => match mcp_cred_refs.get(name.as_str()) {
                Some(target) => manager_services::EnvEntry::credential_ref(name, target),
                None => manager_services::EnvEntry::literal(name, value.clone()),
            },
        })
        .collect();
    env.sort_by(|a, b| a.name.cmp(&b.name));
    let (program, args) = match cmd.argv.split_first() {
        Some((program, rest)) => (PathBuf::from(program), rest.to_vec()),
        None => (PathBuf::new(), Vec::new()),
    };
    manager_services::LaunchSpec {
        program,
        args,
        env,
        workdir: cmd.cwd.clone(),
    }
}

/// The gateway's service launch spec built from configuration alone,
/// without a wizard context: the Settings register action needs a
/// definition before any start-gateway run exists. This mirrors
/// `StartGateway::spawn_command` + `spawn_command_with_env` with the
/// same secret-as-reference transform. It is a starting point, not a
/// second source of truth: the start-gateway step regenerates the
/// definition from its fully resolved spawn command on every run in
/// service mode.
pub fn gateway_service_launch(config: &AppConfig, data_dir: &Path) -> manager_services::LaunchSpec {
    let venv = data_dir.join("python").join("venv");
    let python = if cfg!(windows) {
        let windowless = venv.join("Scripts").join("pythonw.exe");
        if windowless.exists() {
            windowless
        } else {
            venv.join("Scripts").join("python.exe")
        }
    } else {
        venv.join("bin").join("python")
    };
    let cfg_path = data_dir.join("gateway").join("config.yaml");
    let args = vec![
        "-c".to_string(),
        StartGateway::GATEWAY_BOOTSTRAP.to_string(),
        "--config".to_string(),
        cfg_path.display().to_string(),
        "--host".to_string(),
        config.gateway.bind_host.clone(),
        "--port".to_string(),
        config.gateway.port.to_string(),
    ];
    let mut env = vec![
        manager_services::EnvEntry::literal("PYTHONIOENCODING", "utf-8".to_string()),
        manager_services::EnvEntry::literal("PYTHONUTF8", "1".to_string()),
        manager_services::EnvEntry::credential_ref(
            "LITELLM_MASTER_KEY",
            crate::LITELLM_MASTER_KEY_CRED_TARGET,
        ),
    ];
    // PostgreSQL installed (binaries plus cluster or marker — the
    // same ground truth `postgres_installed` uses) → the gateway
    // receives a DATABASE_URL composed at service start.
    let postgres_present = postgres_bin_dir(data_dir).join("pg_ctl.exe").exists()
        && (postgres_marker_path(data_dir).exists()
            || postgres_data_dir(data_dir).join("PG_VERSION").exists());
    if postgres_present {
        env.push(manager_services::EnvEntry {
            name: "DATABASE_URL".to_string(),
            value: manager_services::EnvValue::DatabaseUrl {
                password_credential: crate::POSTGRES_PASSWORD_CRED_TARGET.to_string(),
                port: config.postgres.port,
            },
        });
    }
    if config.gateway.openrouter.enabled {
        if let Some(target) = &config.gateway.openrouter.cred_ref {
            env.push(manager_services::EnvEntry::credential_ref(
                "OPENROUTER_API_KEY",
                target,
            ));
        }
    }
    manager_services::LaunchSpec {
        program: python,
        args,
        env,
        workdir: None,
    }
}

/// The server's own log file, in the app's logs directory alongside
/// the gateway logs (`pg_ctl -l` appends there).
pub fn postgres_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join("postgres.log")
}

/// The database the gateway uses.
pub const POSTGRES_DATABASE: &str = "litellm";

/// Percent-encode a URL userinfo component (RFC 3986): everything
/// outside the unreserved set is `%XX`-escaped. App-generated
/// passwords are hex and pass through unchanged; a password a user
/// sets by hand may contain `@`, `:`, `/`, or spaces, which raw
/// would corrupt the URL's authority parsing (code-review 2.9).
fn encode_userinfo(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The `DATABASE_URL` the gateway receives: the password is
/// percent-encoded for the userinfo slot (app-generated passwords
/// are hex, hence unchanged), the user is the cluster superuser,
/// the database is [`POSTGRES_DATABASE`].
pub fn postgres_database_url(config: &AppConfig, password: &str) -> String {
    format!(
        "postgresql://postgres:{}@127.0.0.1:{}/{POSTGRES_DATABASE}",
        encode_userinfo(password),
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

/// pg_ctl's own start-wait bound (`-t`), in seconds. At expiry
/// pg_ctl reports "server did not start in time" and exits,
/// leaving the postmaster starting in the background — the caller
/// polls for readiness itself (see [`ensure_postgres_running`]).
/// Before this bound existed, the only limit on a slow postmaster
/// startup was the command runner's generic 600 s cap, whose tree
/// kill then killed the starting postmaster itself (field report
/// 2026-10-18).
const PG_CTL_START_WAIT_SECS: u64 = 120;
/// Runner budget for the pg_ctl start command: above pg_ctl's own
/// `-t` bound with headroom, so pg_ctl's own report — not the
/// runner's tree kill — is what ends a slow start.
const POSTGRES_START_COMMAND_BUDGET: std::time::Duration = std::time::Duration::from_secs(180);
/// Readiness poll cadence (the historical poll slept 500 ms).
const POSTGRES_READY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
/// After a clean pg_ctl exit, readiness is expected promptly (the
/// historical 30 × 500 ms poll).
const POSTGRES_CLEAN_START_WAIT: std::time::Duration = std::time::Duration::from_secs(15);
/// After pg_ctl stops waiting (its `-t` bound) the postmaster
/// keeps starting; readiness may still be this far out (crash
/// recovery over a large WAL backlog).
const POSTGRES_SETTLE_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
/// How long to wait for an already-running postmaster (found via
/// `postmaster.pid`) to reach readiness before reporting it stuck.
const POSTGRES_ADOPT_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// The `pg_ctl start` command for the app-managed server, shared
/// by the runtime start ([`ensure_postgres_running`]), the install
/// step's plan, and the start-gateway manual block, so all three
/// run and show the same command.
fn postgres_start_command(data_dir: &Path, port: u16) -> PlannedCommand {
    let port_opt = format!("-p {port} -h 127.0.0.1");
    let log_arg = postgres_log_path(data_dir).display().to_string();
    pg_ctl_command(
        data_dir,
        &[
            "-l",
            &log_arg,
            "-o",
            &port_opt,
            "-t",
            &PG_CTL_START_WAIT_SECS.to_string(),
            "start",
        ],
    )
}

/// `<data_dir>/postgres/data/postmaster.pid` — written by the
/// postmaster at startup and removed on a clean stop; its first
/// line is the postmaster's pid.
fn postmaster_pid_path(data_dir: &Path) -> PathBuf {
    postgres_data_dir(data_dir).join("postmaster.pid")
}

/// The pid recorded in `postmaster.pid`, when the file exists and
/// its first line parses. Absent or unparseable both read as "no
/// recorded postmaster".
async fn read_postmaster_pid(ops: &dyn SystemOps, data_dir: &Path) -> Option<u32> {
    let bytes = ops.read_file(&postmaster_pid_path(data_dir)).await.ok()?;
    let text = String::from_utf8_lossy(&bytes);
    text.lines().next()?.trim().parse().ok()
}

/// Outcome of [`wait_for_postgres_ready`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostgresWait {
    /// `pg_isready` answered success.
    Ready,
    /// The window elapsed without readiness.
    TimedOut,
    /// A postmaster was required to exist and none does (the pid
    /// file is gone, or its recorded pid is not running): nothing
    /// is coming up, so waiting longer cannot help.
    PostmasterGone,
}

/// How often a readiness wait reports itself: every this many
/// polls (~10 s at the 500 ms cadence) the wait streams the new
/// server-log lines into the step log and notes the elapsed time
/// on the live sink. A starting postmaster can spend minutes in
/// crash recovery; without the ticks the wait is a silent window
/// indistinguishable from a hang (field report 2026-10-19).
const POSTGRES_WAIT_TICK_POLLS: u32 = 20;

/// Poll `pg_isready` until it answers, the window elapses, or —
/// with `require_postmaster` — the postmaster disappears (a start
/// that already failed fast must not burn the whole window).
/// While waiting, the postmaster's own log lines stream into the
/// step log (see [`POSTGRES_WAIT_TICK_POLLS`]), so a failing
/// start's FATAL lines surface as they are written instead of
/// only as a tail after the wait has already failed.
async fn wait_for_postgres_ready(
    ctx: &WizardContext,
    data_dir: &Path,
    max: std::time::Duration,
    require_postmaster: bool,
    log: &mut Vec<String>,
) -> PostgresWait {
    wait_for_postgres_ready_ticked(
        ctx,
        data_dir,
        max,
        require_postmaster,
        log,
        POSTGRES_WAIT_TICK_POLLS,
    )
    .await
}

/// [`wait_for_postgres_ready`] with the tick cadence as a
/// parameter, so tests can tick every poll instead of every
/// ~10 s.
async fn wait_for_postgres_ready_ticked(
    ctx: &WizardContext,
    data_dir: &Path,
    max: std::time::Duration,
    require_postmaster: bool,
    log: &mut Vec<String>,
    tick_polls: u32,
) -> PostgresWait {
    let ops = ctx.ops.as_ref();
    let config = &ctx.config;
    let log_path = postgres_log_path(data_dir);
    let started = std::time::Instant::now();
    let mut streamer = FileLineStreamer::new();
    streamer.prime(ops, &log_path).await;
    let mut polls: u32 = 0;
    loop {
        if postgres_ready(ops, config, data_dir).await {
            streamer.stream_new(ctx, ops, &log_path, "postgres: ", log).await;
            return PostgresWait::Ready;
        }
        if require_postmaster {
            match read_postmaster_pid(ops, data_dir).await {
                Some(pid) if ops.process_alive(pid) => {}
                _ => {
                    streamer.stream_new(ctx, ops, &log_path, "postgres: ", log).await;
                    return PostgresWait::PostmasterGone;
                }
            }
        }
        if started.elapsed() >= max {
            return PostgresWait::TimedOut;
        }
        tokio::time::sleep(POSTGRES_READY_POLL_INTERVAL).await;
        polls += 1;
        if tick_polls > 0 && polls.is_multiple_of(tick_polls) {
            streamer.stream_new(ctx, ops, &log_path, "postgres: ", log).await;
            ctx.emit_log(&format!(
                "waiting for PostgreSQL on 127.0.0.1:{} — {:.0}s elapsed",
                config.postgres.port,
                started.elapsed().as_secs_f64()
            ));
        }
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
    ctx: &WizardContext,
    log: &mut Vec<String>,
) -> Result<String, String> {
    let ops = ctx.ops.as_ref();
    let config = &ctx.config;
    let data_dir = ctx.data_dir.as_path();
    let password = crate::ensure_postgres_password(ops)
        .await
        .map_err(|e| format!("could not obtain the PostgreSQL password: {e}"))?;
    let url = postgres_database_url(config, &password);
    if postgres_ready(ops, config, data_dir).await {
        push_log(
            ctx,
            log,
            format!(
                "PostgreSQL already running on 127.0.0.1:{}",
                config.postgres.port
            ),
        );
        return Ok(url);
    }
    // A postmaster the readiness probe cannot see yet: one left
    // running by an earlier session (PostgreSQL stops with the
    // gateway stop, but an app crash, a killed process tree, or a
    // machine sleep can leave it behind) that is still starting —
    // crash recovery after an unclean shutdown runs before it
    // accepts connections, so `pg_isready` fails while it works.
    // Starting a second postmaster against the same data directory
    // cannot help (PostgreSQL refuses a second lock holder), so an
    // existing one is adopted and awaited. A pid file whose pid is
    // dead — or has been recycled by an unrelated process (Windows
    // pid reuse; the image check tells the two apart) — is stale:
    // remove it, so the fresh start below is not refused over a
    // lock file no live server owns.
    if let Some(pid) = read_postmaster_pid(ops, data_dir).await {
        if !ops.process_alive(pid) {
            push_log(
                ctx,
                log,
                format!("removing stale postmaster.pid: recorded pid {pid} is not running"),
            );
            let _ = ops.remove_file(&postmaster_pid_path(data_dir)).await;
        } else if ops.process_image_is(pid, "postgres.exe") == Some(false) {
            push_log(
                ctx,
                log,
                format!(
                    "removing stale postmaster.pid: recorded pid {pid} now belongs to a different program (pid reuse)"
                ),
            );
            let _ = ops.remove_file(&postmaster_pid_path(data_dir)).await;
        } else {
            push_log(
                ctx,
                log,
                format!(
                    "PostgreSQL postmaster (pid {pid}) is running but not accepting connections yet — waiting for it instead of starting a second server"
                ),
            );
            match wait_for_postgres_ready(ctx, data_dir, POSTGRES_ADOPT_WAIT, true, log).await {
                PostgresWait::Ready => {
                    push_log(
                        ctx,
                        log,
                        format!(
                            "PostgreSQL is accepting connections on 127.0.0.1:{} (existing postmaster, pid {pid})",
                            config.postgres.port
                        ),
                    );
                    return Ok(url);
                }
                PostgresWait::PostmasterGone => {
                    push_log(
                        ctx,
                        log,
                        "the existing postmaster exited while starting; starting PostgreSQL afresh"
                            .to_string(),
                    );
                    let _ = ops.remove_file(&postmaster_pid_path(data_dir)).await;
                }
                PostgresWait::TimedOut => {
                    let tail = postgres_log_tail(ops, data_dir).await;
                    return Err(format!(
                        "PostgreSQL postmaster (pid {pid}) is running but did not accept connections within {} s — it may be stuck in startup; server log tail:\n{tail}",
                        POSTGRES_ADOPT_WAIT.as_secs()
                    ));
                }
            }
        }
    }
    // pg_ctl's wait is bounded twice over: its own `-t` (see
    // postgres_start_command) and the runner budget above it, set
    // so pg_ctl's report — not the runner's tree kill — ends a slow
    // start. The old invocation carried neither bound: a slow
    // postmaster startup ran into the command runner's generic
    // 600 s cap, the resulting tree kill killed the starting
    // postmaster itself (guaranteeing another crash recovery on
    // the next attempt), and the failure on that path carried no
    // server log tail (field report 2026-10-18: "timed out after
    // 600s and was stopped", undiagnosable from the outcome).
    // Every failure below now carries the tail.
    let start = postgres_start_command(data_dir, config.postgres.port);
    let start_failure = match run_command_logged(
        ctx,
        log,
        &start,
        Some(POSTGRES_START_COMMAND_BUDGET),
    )
    .await
    {
        Ok(out) if out.exit_code == 0 => {
            push_log(
                ctx,
                log,
                format!(
                    "PostgreSQL started on 127.0.0.1:{} (log: {})",
                    config.postgres.port,
                    postgres_log_path(data_dir).display()
                ),
            );
            None
        }
        Ok(out) => Some(format!(
            "pg_ctl exit {}: {}",
            out.exit_code,
            out.stderr.trim()
        )),
        Err(e) => Some(e.to_string()),
    };
    // The definitive signal, either way. After a clean pg_ctl exit
    // the server should already answer (the historical brief
    // poll). After a pg_ctl failure the postmaster may still be
    // starting — pg_ctl stops waiting at its `-t` bound while
    // startup continues in the background — so poll longer, but
    // stop early when no postmaster exists anymore (a fast FATAL
    // must not burn the window).
    let wait = wait_for_postgres_ready(
        ctx,
        data_dir,
        if start_failure.is_none() {
            POSTGRES_CLEAN_START_WAIT
        } else {
            POSTGRES_SETTLE_WAIT
        },
        start_failure.is_some(),
        log,
    )
    .await;
    match wait {
        PostgresWait::Ready => {
            if start_failure.is_some() {
                push_log(
                    ctx,
                    log,
                    "PostgreSQL became ready after pg_ctl stopped waiting — startup completed in the background"
                        .to_string(),
                );
            }
            Ok(url)
        }
        _ => {
            let tail = postgres_log_tail(ops, data_dir).await;
            match start_failure {
                Some(failure) => Err(format!(
                    "PostgreSQL failed to start ({failure}) — server log tail:\n{tail}"
                )),
                None => Err(format!(
                    "PostgreSQL did not become ready on 127.0.0.1:{} — server log tail:\n{tail}",
                    config.postgres.port
                )),
            }
        }
    }
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
/// Remove-on-drop guard for the initdb password file: the file
/// holds the cluster superuser password in plaintext and must not
/// outlive the initdb run — including when the step future is
/// dropped mid-run (app closed, step timeout), which no awaited
/// cleanup line can cover. Removal is synchronous `std::fs`
/// because `Drop` cannot await the ops trait; the step also removes
/// the file through the ops on its normal paths (under the fake
/// ops the file exists only in the fake filesystem, where the
/// ops removal is the one that lands).
struct PwFileGuard(PathBuf);

impl Drop for PwFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

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
        vec![
            PlannedCommand::new(vec![
                "download".to_string(),
                pin.url(),
                postgres_zip_path(&ctx.data_dir, pin).display().to_string(),
            ]),
            self.initdb_command(ctx),
            postgres_start_command(&ctx.data_dir, ctx.config.postgres.port),
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
                push_log(ctx, &mut log, format!("downloading {url}"));
                let progress = ProgressEmitter::new(ctx, pin.asset.clone());
                let dl_started = std::time::Instant::now();
                match ctx
                    .ops
                    .download(&url, &zip, &|d, t| progress.progress(d, t))
                    .await
                {
                    Ok(bytes) => push_log(
                        ctx,
                        &mut log,
                        format!("downloaded {bytes} bytes in {}", fmt_elapsed(dl_started)),
                    ),
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
            push_log(
                ctx,
                &mut log,
                format!(
                    "extracting PostgreSQL binaries from {} to {}",
                    zip.display(),
                    postgres_root(&ctx.data_dir).display()
                ),
            );
            let extract_started = std::time::Instant::now();
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
            push_log(
                ctx,
                &mut log,
                format!(
                    "extracted bin/lib/share/include to {} in {}",
                    postgres_root(&ctx.data_dir).display(),
                    fmt_elapsed(extract_started)
                ),
            );
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
            // The password file holds the cluster superuser
            // password in plaintext; it must not outlive initdb.
            // The explicit removal below covers the normal paths —
            // this guard covers the step future being DROPPED
            // mid-initdb (app closed, step timeout), which skipped
            // the removal entirely (code-review 2.10). Synchronous
            // std::fs removal: Drop cannot await the ops trait.
            let _pwfile_guard = PwFileGuard(pwfile.clone());
            let initdb = self.initdb_command(ctx);
            let result = run_command_logged(ctx, &mut log, &initdb, None).await;
            // The password file is single-use: always remove it.
            let _ = ctx.ops.remove_file(&pwfile).await;
            match result {
                Ok(out) if out.exit_code == 0 => {
                    push_log(
                        ctx,
                        &mut log,
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
        if let Err(e) = ensure_postgres_running(ctx, &mut log).await {
            return Ok(StepOutcome::failed(e, log));
        }
        let mut createdb = self.createdb_command(ctx);
        createdb.env.insert("PGPASSWORD".to_string(), password);
        match run_command_logged(ctx, &mut log, &createdb, None).await {
            Ok(out) if out.exit_code == 0 => {
                push_log(ctx, &mut log, format!("database {POSTGRES_DATABASE} created"));
            }
            Ok(out) if out.stderr.contains("already exists") => {
                push_log(
                    ctx,
                    &mut log,
                    format!("database {POSTGRES_DATABASE} already exists"),
                );
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

/// Budget for the quick verification probes (interpreter version,
/// package import, package version): each is a version print or an
/// import, done in seconds when healthy. They previously ran at
/// the ops layer's generic 600 s command budget, so a wedged probe
/// held the step silently for ten minutes before any repair was
/// even chosen.
const PROBE_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Step 6: bootstrap the managed Python env (uv), generate `config.yaml`
/// from the app config, spawn LiteLLM on :4000, health-check.
pub struct StartGateway {
    pub health_retries: u32,
    pub health_interval_ms: u64,
    /// How long the database path keeps confirming health after the
    /// first healthy probe before the step reports success
    /// ([`StartGateway::confirm_sustained_health`]). Zero disables
    /// the confirmation window (tests); production uses the
    /// default.
    pub db_sustain_ms: u64,
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
            // Both known gateway-with-database defects strike about
            // a second after a plain health probe first succeeds —
            // the gateway answers, then dies. Ten seconds of
            // continued answers is far past that window.
            db_sustain_ms: 10_000,
        }
    }
}

/// Escape a value for interpolation inside a PowerShell
/// double-quoted string. PowerShell's escape character is the
/// backtick: `` ` `` doubles itself, and backtick before `"` or `$`
/// stops the quote ending the string and the dollar starting a
/// variable/subexpression expansion. A backslash escapes nothing
/// in PowerShell — the previous `\"` rendering of an embedded
/// quote actually TERMINATED the string (code-review 2.7).
fn ps_dq(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '`' => out.push_str("``"),
            '"' => out.push_str("`\""),
            '$' => out.push_str("`$"),
            _ => out.push(c),
        }
    }
    out
}

/// Join an argv into one paste-able PowerShell command line:
/// arguments containing whitespace, a quote, a backtick, or a `$`
/// are double-quoted with their contents escaped via [`ps_dq`];
/// everything else passes through bare.
fn ps_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty() {
                "\"\"".to_string()
            } else if a
                .chars()
                .any(|c| c.is_whitespace() || c == '"' || c == '`' || c == '$')
            {
                format!("\"{}\"", ps_dq(a))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Render one executable command line for the manual start block.
/// PowerShell parses a line whose first token is a quoted string as
/// an expression, not a command, and rejects it ("Unexpected token")
/// — the call operator `&` is required before a quoted executable
/// path, and harmless before a bare one, so every executable line
/// in the block carries it unconditionally. (An operator pasting
/// the block by hand hit exactly this, 2026-10-16.)
fn ps_command_line(argv: &[String]) -> String {
    format!("& {}", ps_join(argv))
}

impl StartGateway {
    fn venv_dir(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("python").join("venv")
    }

    /// The commands to start the gateway by hand, attached to this
    /// step's failure outcomes ([`StepOutcome::manual_command`]) so
    /// an operator whose start failed can run them in PowerShell —
    /// getting a gateway up and seeing its errors directly — instead
    /// of being stuck behind the wizard. Contents, in run order:
    /// the app-managed PostgreSQL start line (only when PostgreSQL
    /// is installed), the secret environment the spawn needs (names
    /// and where the values live — Windows Credential Manager —
    /// never the values), and the exact spawn command
    /// [`Self::spawn_command`] builds, with this install's real
    /// paths, host, and port. Built once per run: whether PostgreSQL
    /// is installed cannot change during this step (installing it is
    /// a different step).
    async fn manual_start_block(&self, ctx: &WizardContext) -> String {
        let mut lines = vec![
            "# Start the gateway manually (PowerShell):".to_string(),
            "# Secret values are read from Windows Credential Manager; they are never written to disk."
                .to_string(),
        ];
        if postgres_installed(ctx.ops.as_ref(), &ctx.data_dir).await {
            let pg = postgres_start_command(&ctx.data_dir, ctx.config.postgres.port);
            lines.push(ps_command_line(&pg.argv));
            // The REAL port, not a placeholder: a block that says
            // "paste this and it works" must not hand the operator a
            // <port> token to hunt down (code-review 2.6).
            lines.push(format!(
                "$env:DATABASE_URL = \"postgresql://postgres:<password>@127.0.0.1:{}/{POSTGRES_DATABASE}\"  # <password>: Credential Manager '{}'",
                ctx.config.postgres.port,
                crate::POSTGRES_PASSWORD_CRED_TARGET
            ));
            // Same watchdog disable the app spawn sets (see
            // StartGateway::prisma_watchdog_must_be_disabled): a
            // manual start on Windows with a database otherwise
            // dies about a second after startup, silently.
            lines.push("$env:PRISMA_HEALTH_WATCHDOG_ENABLED = \"false\"".to_string());
        }
        lines.push(format!(
            "$env:LITELLM_MASTER_KEY = \"<value from Windows Credential Manager: '{}'>\"",
            crate::LITELLM_MASTER_KEY_CRED_TARGET
        ));
        let tier = &ctx.config.gateway.openrouter;
        if tier.enabled {
            if let Some(cred_ref) = tier.cred_ref.as_deref().filter(|r| !r.is_empty()) {
                lines.push(format!(
                    "$env:OPENROUTER_API_KEY = \"<value from Windows Credential Manager: '{cred_ref}'>\""
                ));
            }
        }
        // MCP gateway servers (design section 4.2): a manual start must
        // behave like an app start -- the stdio flag when any
        // Gateway-exposed stdio server is configured (the manual
        // block is config-derived, like the plan; a server whose
        // credential is missing will simply not load, as at an app
        // start where it is not armed), plus a credential note per
        // Gateway server naming each env key it feeds. Values are
        // never shown, mirroring the master-key/OpenRouter lines.
        let gateway_mcp: Vec<&manager_config::McpServerConfig> = ctx
            .config
            .mcp
            .servers
            .iter()
            .filter(|s| s.exposure == manager_config::McpExposure::Gateway)
            .collect();
        if gateway_mcp
            .iter()
            .any(|s| s.transport == manager_config::McpTransport::Stdio)
        {
            lines.push("$env:LITELLM_ENABLE_MCP_STDIO = \"true\"".to_string());
        }
        for server in &gateway_mcp {
            if let Some(cred_ref) = server.cred_ref.as_deref().filter(|r| !r.is_empty()) {
                for key in &server.env_keys {
                    lines.push(format!(
                        "$env:{key} = \"<value from Windows Credential Manager: '{cred_ref}'>\"  # MCP server '{}'",
                        server.id
                    ));
                }
            }
        }
        // SearXNG search provider (design §6.3): the instance URL is
        // config, not a secret, so the manual block carries the real
        // value — a manual start must behave like an app start.
        let searxng = &ctx.config.mcp.searxng;
        if searxng.enabled && !searxng.api_base.trim().is_empty() {
            lines.push(format!(
                "$env:SEARXNG_API_BASE = \"{}\"",
                searxng.api_base.trim()
            ));
        }
        let spawn = self.spawn_command(ctx);
        // The spawn's own environment is not secret (UTF-8 stdio
        // forcing); emit it exactly as the step sets it.
        let mut env: Vec<(&String, &String)> = spawn.env.iter().collect();
        env.sort();
        for (k, v) in env {
            lines.push(format!("$env:{k} = \"{v}\""));
        }
        lines.push(ps_command_line(&spawn.argv));
        lines.join("\n")
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
        // ensure_uv has no step log of its own — the download's
        // progress goes to the live sink only.
        ctx.emit_log(&format!("downloading {url} (uv bootstrap)"));
        let progress = ProgressEmitter::new(ctx, "uv");
        let dl_started = std::time::Instant::now();
        ctx.ops
            .download(&url, &archive, &|d, t| progress.progress(d, t))
            .await
            .map_err(|e| {
                StepOutcome::failed(
                    format!("uv bootstrap download failed: {e}"),
                    vec![format!("{e:?}")],
                )
            })?;
        ctx.emit_log(&format!(
            "uv bootstrap downloaded in {}",
            fmt_elapsed(dl_started)
        ));
        ctx.emit_log(&format!("extracting the uv bootstrap archive to {}", dir.display()));
        let extract_started = std::time::Instant::now();
        ctx.ops.extract_zip(&archive, &dir).await.map_err(|e| {
            StepOutcome::failed(
                format!("uv bootstrap extraction failed: {e}"),
                vec![format!("{e:?}")],
            )
        })?;
        ctx.emit_log(&format!(
            "uv bootstrap extracted in {}",
            fmt_elapsed(extract_started)
        ));
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

    /// The interpreter probe WITH its evidence: on failure the
    /// second element says why (no interpreter, spawn error, exit
    /// code + output tail, or output with no version in it), so
    /// the caller can record the actual defect in the step log
    /// before choosing a repair. Probes used to answer a bare
    /// bool/None, which made a corrupted venv indistinguishable
    /// from a missing one — and the repair for "missing" is the
    /// full rebuild. Bounded by [`PROBE_COMMAND_TIMEOUT`].
    async fn venv_python_version_probe(
        &self,
        ctx: &WizardContext,
    ) -> (Option<String>, Option<String>) {
        let python = self.venv_python(ctx);
        if !ctx.ops.file_exists(&python).await {
            return (None, Some("the venv interpreter does not exist".into()));
        }
        match ctx
            .ops
            .run_command_with_timeout(&self.version_probe_command(ctx), PROBE_COMMAND_TIMEOUT)
            .await
        {
            Ok(out) if out.success() => match parse_two_part_version(&out.stdout) {
                Some(v) => (Some(v), None),
                None => (
                    None,
                    Some(format!(
                        "no version in the interpreter's output: {}",
                        output_tail_lines(out.stdout.trim(), 2).join(" | ")
                    )),
                ),
            },
            Ok(out) => (None, Some(probe_failure_detail(&out))),
            Err(e) => (None, Some(format!("the interpreter could not run: {e}"))),
        }
    }

    /// The import probe WITH its evidence (see
    /// [`Self::venv_python_version_probe`]): on failure the second
    /// element carries the import error itself — the line that
    /// distinguishes a missing package from a corrupted one.
    async fn litellm_import_probe(&self, ctx: &WizardContext) -> (bool, Option<String>) {
        match ctx
            .ops
            .run_command_with_timeout(&self.litellm_probe_command(ctx), PROBE_COMMAND_TIMEOUT)
            .await
        {
            Ok(out) if out.success() => (true, None),
            Ok(out) => (false, Some(probe_failure_detail(&out))),
            Err(e) => (false, Some(format!("the interpreter could not run: {e}"))),
        }
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
            // clean 401. Found on real hardware 2026-10-15. The
            // version is pinned exactly: prisma-client-py >= 0.13
            // breaks LiteLLM's PrismaWrapper (see
            // crate::PRISMA_PINNED_VERSION).
            format!("prisma=={}", crate::PRISMA_PINNED_VERSION),
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

    /// Whether the gateway spawn must carry
    /// `PRISMA_HEALTH_WATCHDOG_ENABLED=false`. LiteLLM's Prisma
    /// health watchdog (`_poll_engine_proc` in the proxy utils,
    /// introduced by LiteLLM PR #21899) probes the query-engine
    /// process with `os.kill(pid, 0)`. On Linux/macOS signal 0 is a
    /// harmless existence probe; on Windows CPython routes
    /// `os.kill` to `TerminateProcess`, so the watchdog's first poll
    /// KILLS the engine it meant to check. The proxy reads the dead
    /// engine as a lost database connection and shuts Uvicorn down
    /// cleanly about a second after startup, logging no error —
    /// which is also why a standalone Prisma client (no watchdog)
    /// stays connected indefinitely. BerriAI/litellm issues #25260,
    /// #30847, #26886; field-verified 2026-10-16: with the flag set
    /// the gateway stays up. The watchdog only exists on the
    /// database path, and the defect is Windows-only, so the flag
    /// is set exactly then; elsewhere the watchdog works as
    /// intended and stays enabled. Downside of disabling it: no
    /// automatic engine restart if PostgreSQL or the engine dies
    /// later — the gateway needs a restart to recover its database,
    /// acceptable for an app-supervised single-node gateway.
    fn prisma_watchdog_must_be_disabled(on_windows: bool, database_url: Option<&str>) -> bool {
        on_windows && database_url.is_some()
    }

    async fn spawn_command_with_env(
        &self,
        ctx: &WizardContext,
        database_url: Option<&str>,
        mcp_env: &[(String, String)],
        mcp_stdio_enabled: bool,
    ) -> Result<(PlannedCommand, Option<String>), StepOutcome> {
        let mut cmd = self.spawn_command(ctx);
        // The master key is unconditional: current LiteLLM refuses
        // to boot without one. Failing to obtain it fails the step
        // here rather than spawning a gateway that exits at startup.
        let master_key = self.ensure_master_key(ctx).await?;
        cmd.env.insert("LITELLM_MASTER_KEY".to_string(), master_key);
        // MCP gateway servers (design §4.2): the stdio opt-in flag
        // when at least one armed stdio Gateway server exists, and
        // each armed server's credential under each of its env-key
        // names (the generated YAML references them as
        // os.environ/<NAME>). The values were read once by the
        // arming pass in run(); they are never logged.
        if mcp_stdio_enabled {
            cmd.env.insert(
                "LITELLM_ENABLE_MCP_STDIO".to_string(),
                "true".to_string(),
            );
        }
        for (key, value) in mcp_env {
            cmd.env.insert(key.clone(), value.clone());
        }
        // SearXNG search provider (design §6.3): LiteLLM reads the
        // instance URL from SEARXNG_API_BASE in the process
        // environment (the generated YAML's search_tools entry
        // carries the same value as its api_base). The base is
        // config, not a secret, so it travels in plain form like
        // the other non-secret spawn values.
        let searxng = &ctx.config.mcp.searxng;
        if searxng.enabled && !searxng.api_base.trim().is_empty() {
            cmd.env.insert(
                "SEARXNG_API_BASE".to_string(),
                searxng.api_base.trim().to_string(),
            );
        }
        // The database URL is present exactly when the app-managed
        // PostgreSQL is installed and running (the caller started
        // it). With it, LiteLLM migrates its schema at startup and
        // the admin UI can be logged into; without it the gateway
        // runs database-free, as before PostgreSQL support.
        if let Some(url) = database_url {
            cmd.env.insert("DATABASE_URL".to_string(), url.to_string());
            // LiteLLM's Prisma health watchdog kills its own query
            // engine on Windows (see
            // Self::prisma_watchdog_must_be_disabled); with a
            // database configured it must not run.
            if Self::prisma_watchdog_must_be_disabled(cfg!(target_os = "windows"), database_url) {
                cmd.env.insert(
                    "PRISMA_HEALTH_WATCHDOG_ENABLED".to_string(),
                    "false".to_string(),
                );
            }
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
        if ctx.config.supervision.mode == SupervisionMode::Service {
            // The SCM owns the previous gateway process; killing the
            // recorded pid would kill the service host's child out
            // from under it and leave the service state lying. Stop
            // the service instead — its host removes the pid file.
            match ctx
                .ops
                .service_stop(manager_services::GATEWAY_SERVICE_NAME)
                .await
            {
                Ok(()) => {
                    log.push("stopped previous gateway service".to_string());
                    let _ = ctx.ops.remove_file(&pid_path).await;
                    return Ok(());
                }
                Err(e) => {
                    return Err(StepOutcome::failed(
                        format!(
                            "could not stop the previous gateway service: {e}; \
                             refusing to start a duplicate on :{}",
                            ctx.config.gateway.port
                        ),
                        log.clone(),
                    ));
                }
            }
        }
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

    /// Service-mode start: regenerate the gateway service definition
    /// from the fully built spawn command and start the service,
    /// returning the gateway child's pid (recorded by the service
    /// host in the gateway pid file) for the health checks.
    async fn spawn_gateway_as_service(
        &self,
        ctx: &WizardContext,
        spawn: &PlannedCommand,
        log: &mut Vec<String>,
    ) -> Result<u32, StepOutcome> {
        let def = manager_services::gateway_definition(
            gateway_launch_spec(spawn, &ctx.config),
            self.gateway_stdout_log(ctx),
            self.gateway_stderr_log(ctx),
            crate::gateway_pid_path(&ctx.data_dir),
        );
        match manager_services::write_definition(&ctx.data_dir, &def) {
            Ok(path) => log.push(format!(
                "wrote gateway service definition {}",
                path.display()
            )),
            Err(e) => {
                return Err(StepOutcome::failed(
                    format!("could not write the gateway service definition: {e}"),
                    log.clone(),
                ))
            }
        }
        ctx.ops
            .service_start_gateway(&def)
            .await
            .map_err(|e| {
                StepOutcome::failed(
                    format!("gateway service start failed: {e}"),
                    log.clone(),
                )
            })
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

    /// The directory holding the venv's executables (`Scripts\` on
    /// Windows, `bin/` elsewhere): the interpreter lives there, and
    /// so do the console scripts installed into the venv.
    fn venv_scripts_dir(&self, ctx: &WizardContext) -> PathBuf {
        self.venv_python(ctx)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.venv_dir(ctx))
    }

    /// PATH for a venv child process: the venv's script directory
    /// first, then the inherited PATH. The Prisma CLI spawns its
    /// Python generator, `prisma-client-py`, BY NAME during
    /// `prisma generate`; that executable lives in the venv's script
    /// directory, which is not on PATH just because the interpreter
    /// was invoked by absolute path — without the prepend, generate
    /// fails with "spawn prisma-client-py ENOENT" (found on real
    /// hardware 2026-10-16). Every command that drives the Prisma
    /// toolchain gets this PATH (see [`Self::with_prisma_env`]).
    fn venv_path_with_scripts(&self, ctx: &WizardContext) -> String {
        let scripts = self.venv_scripts_dir(ctx);
        let sep = if cfg!(windows) { ";" } else { ":" };
        match std::env::var_os("PATH") {
            Some(existing) if !existing.is_empty() => {
                format!("{}{sep}{}", scripts.display(), existing.to_string_lossy())
            }
            _ => scripts.display().to_string(),
        }
    }

    /// Attach the environment the Prisma commands run with: the
    /// gateway's DATABASE_URL (the LiteLLM schema's datasource reads
    /// it from the environment) and the PATH from
    /// [`Self::venv_path_with_scripts`].
    fn with_prisma_env(
        &self,
        ctx: &WizardContext,
        database_url: &str,
        mut cmd: PlannedCommand,
    ) -> PlannedCommand {
        cmd.env
            .insert("DATABASE_URL".to_string(), database_url.to_string());
        cmd.env
            .insert("PATH".to_string(), self.venv_path_with_scripts(ctx));
        cmd
    }

    /// Probe that succeeds only when the venv's Prisma client is
    /// generated AND functional standalone: it instantiates the
    /// client and connects it against the real DATABASE_URL. A
    /// plain import is weaker evidence than the gateway needs —
    /// the connect exercises the generated client, the engine, and
    /// the database in one pass. Until `prisma generate` has run
    /// at all, the import inside this probe raises "The Client
    /// hasn't been generated yet" and the probe fails the same way
    /// the gateway would. Scope limit: this probe cannot see
    /// defects inside LiteLLM's own Prisma wrapper (the
    /// '_Prisma__engine' failure tracked in
    /// docs/debugging/gateway-start-troubleshooting.md passes a
    /// standalone connect) — that class is closed by the version
    /// pin enforced in [`Self::ensure_prisma_pin`], not by
    /// detection here.
    fn prisma_client_probe_command(
        &self,
        ctx: &WizardContext,
        database_url: &str,
    ) -> PlannedCommand {
        const SCRIPT: &str = concat!(
            "import asyncio\n",
            "from prisma import Prisma\n",
            "\n",
            "async def _probe():\n",
            "    client = Prisma()\n",
            "    await client.connect()\n",
            "    await client.disconnect()\n",
            "\n",
            "asyncio.run(_probe())\n",
        );
        self.with_prisma_env(
            ctx,
            database_url,
            PlannedCommand::new(vec![
                self.venv_python(ctx).display().to_string(),
                "-c".to_string(),
                SCRIPT.to_string(),
            ]),
        )
    }

    /// Run the connect probe ([`Self::prisma_client_probe_command`]):
    /// None when the client is healthy, otherwise a one-line
    /// description of the failure for the step log and, at the end
    /// of the recovery sequence, the step's failure message.
    async fn prisma_probe_failure(
        &self,
        ctx: &WizardContext,
        database_url: &str,
    ) -> Option<String> {
        let probe = self.prisma_client_probe_command(ctx, database_url);
        // A healthy connect answers in well under a second against
        // the PostgreSQL the step just started; the budget only
        // bounds a wedged engine startup.
        const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
        match ctx
            .ops
            .run_command_with_timeout(&probe, PROBE_TIMEOUT)
            .await
        {
            Ok(out) if out.success() => None,
            Ok(out) => {
                let detail = if out.stderr.trim().is_empty() {
                    out.stdout.trim().to_string()
                } else {
                    out.stderr.trim().to_string()
                };
                Some(format!("exit {}: {}", out.exit_code, detail))
            }
            Err(e) => Some(format!("{e}")),
        }
    }

    /// Command printing the path of the schema the installed
    /// LiteLLM ships (`<litellm>/proxy/schema.prisma`), resolved
    /// from the package itself rather than a guessed layout.
    fn prisma_schema_command(&self, ctx: &WizardContext, database_url: &str) -> PlannedCommand {
        self.with_prisma_env(
            ctx,
            database_url,
            PlannedCommand::new(vec![
                self.venv_python(ctx).display().to_string(),
                "-c".to_string(),
                "import litellm, pathlib; print(pathlib.Path(litellm.__file__).parent / 'proxy' / 'schema.prisma')"
                    .to_string(),
            ]),
        )
    }

    /// `prisma generate` against the LiteLLM schema.
    fn prisma_generate_command(
        &self,
        ctx: &WizardContext,
        database_url: &str,
        schema: &Path,
    ) -> PlannedCommand {
        self.with_prisma_env(
            ctx,
            database_url,
            PlannedCommand::new(vec![
                self.venv_python(ctx).display().to_string(),
                "-m".to_string(),
                "prisma".to_string(),
                "generate".to_string(),
                "--schema".to_string(),
                schema.display().to_string(),
            ]),
        )
    }

    /// Resolve the LiteLLM schema path inside the venv, failing the
    /// step (with a clear message) when it cannot be located: a
    /// gateway started without generation would die at startup, so
    /// there is no useful fallback from here.
    async fn resolve_prisma_schema(
        &self,
        ctx: &WizardContext,
        database_url: &str,
        log: &[String],
    ) -> Result<PathBuf, StepOutcome> {
        let cmd = self.prisma_schema_command(ctx, database_url);
        let out = match ctx.ops.run_command(&cmd).await {
            Ok(out) if out.success() => out,
            Ok(out) => {
                return Err(StepOutcome::failed(
                    format!(
                        "could not locate the LiteLLM Prisma schema (exit {}): {}",
                        out.exit_code,
                        out.stderr.trim()
                    ),
                    log.to_vec(),
                ))
            }
            Err(e) => {
                return Err(StepOutcome::failed(
                    format!("could not locate the LiteLLM Prisma schema: {e}"),
                    log.to_vec(),
                ))
            }
        };
        let raw = out.stdout.trim();
        if raw.is_empty() {
            return Err(StepOutcome::failed(
                "could not locate the LiteLLM Prisma schema: the venv's litellm package did not report a schema path",
                log.to_vec(),
            ));
        }
        let path = PathBuf::from(raw);
        if !ctx.ops.file_exists(&path).await {
            return Err(StepOutcome::failed(
                format!(
                    "the LiteLLM Prisma schema was not found at {} — the litellm installation in the venv is incomplete; re-running this step reinstalls it",
                    path.display()
                ),
                log.to_vec(),
            ));
        }
        Ok(path)
    }

    /// Run `prisma generate` against the schema, logging the
    /// stage; failure is the step's failure. First generation
    /// downloads the Prisma engines and a Node runtime, so it gets
    /// the same budget as a cold package install.
    async fn run_prisma_generate(
        &self,
        ctx: &WizardContext,
        database_url: &str,
        schema: &Path,
        log: &mut Vec<String>,
    ) -> Result<(), StepOutcome> {
        let generate = self.prisma_generate_command(ctx, database_url, schema);
        const GENERATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);
        match run_command_logged(ctx, log, &generate, Some(GENERATE_TIMEOUT)).await {
            Ok(out) if out.success() => {
                if !out.stdout.trim().is_empty() {
                    log.push(out.stdout.trim().to_string());
                }
                log.push("Prisma client generated".to_string());
                Ok(())
            }
            Ok(out) => {
                let output = if out.stderr.trim().is_empty() {
                    out.stdout.trim()
                } else {
                    out.stderr.trim()
                };
                Err(StepOutcome::failed(
                    format!(
                        "prisma generate failed (exit {}): {}",
                        out.exit_code, output
                    ),
                    log.clone(),
                ))
            }
            Err(e) => Err(StepOutcome::failed(
                format!("prisma generate could not run: {e}"),
                log.clone(),
            )),
        }
    }

    /// Probe printing the installed prisma-client-py version, or
    /// nothing usable when the package is absent or broken.
    fn prisma_version_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            self.venv_python(ctx).display().to_string(),
            "-c".to_string(),
            "import importlib.metadata; print(importlib.metadata.version('prisma'))".to_string(),
        ])
    }

    /// The prisma version probe WITH its evidence (see
    /// [`Self::venv_python_version_probe`]): on failure the second
    /// element says why, so a repair decision can be logged with
    /// its cause. The parse tolerates noise around the version
    /// line ([`parse_semver_version`]) — output pollution used to
    /// read as "version unknown" and trigger the full pin repair
    /// (package removal + reinstall + regenerate) on every start.
    async fn installed_prisma_version_probe(
        &self,
        ctx: &WizardContext,
    ) -> (Option<String>, Option<String>) {
        match ctx
            .ops
            .run_command_with_timeout(&self.prisma_version_command(ctx), PROBE_COMMAND_TIMEOUT)
            .await
        {
            Ok(out) if out.success() => match parse_semver_version(&out.stdout) {
                Some(v) => (Some(v), None),
                None => (
                    None,
                    Some(format!(
                        "no version in the probe output: {}",
                        output_tail_lines(out.stdout.trim(), 2).join(" | ")
                    )),
                ),
            },
            Ok(out) => (None, Some(probe_failure_detail(&out))),
            Err(e) => (None, Some(format!("the probe could not run: {e}"))),
        }
    }

    /// Probe printing the installed LiteLLM version, or nothing
    /// usable when the package is absent or broken. Mirrors
    /// [`Self::prisma_version_command`] (MCP design §4.3).
    fn litellm_version_command(&self, ctx: &WizardContext) -> PlannedCommand {
        PlannedCommand::new(vec![
            self.venv_python(ctx).display().to_string(),
            "-c".to_string(),
            "import importlib.metadata; print(importlib.metadata.version('litellm'))".to_string(),
        ])
    }

    /// The venv's installed LiteLLM version ("1.105.1"), or None
    /// when it cannot be determined (package missing, metadata
    /// broken) — callers record None as "not measured" and never
    /// guess. Phase 0 records the probed version in the spawn
    /// state; the MCP floor check against
    /// [`crate::LITELLM_MCP_FLOOR_VERSION`] is enforced from
    /// Phase 2, when gateway-registered stdio servers exist.
    async fn installed_litellm_version(&self, ctx: &WizardContext) -> Option<String> {
        match ctx
            .ops
            .run_command_with_timeout(&self.litellm_version_command(ctx), PROBE_COMMAND_TIMEOUT)
            .await
        {
            Ok(out) if out.success() => parse_semver_version(&out.stdout),
            _ => None,
        }
    }

    /// The `prisma` package directory inside the venv's
    /// site-packages: where the library AND the generated client
    /// live (`Lib\site-packages` on Windows, `lib/pythonX.Y/
    /// site-packages` elsewhere, matching the pinned interpreter).
    fn prisma_package_dir(&self, ctx: &WizardContext) -> PathBuf {
        let venv = self.venv_dir(ctx);
        if cfg!(target_os = "windows") {
            venv.join("Lib").join("site-packages").join("prisma")
        } else {
            venv.join("lib")
                .join(format!("python{}", crate::GATEWAY_PYTHON_VERSION))
                .join("site-packages")
                .join("prisma")
        }
    }

    /// Reinstall exactly the pinned prisma-client-py into the venv.
    /// `--reinstall-package` forces replacement even when the
    /// resolver considers some prisma already installed (its
    /// dist-info survives the package-directory removal in
    /// [`Self::ensure_prisma_pin`]).
    fn prisma_pin_command(&self, ctx: &WizardContext, uv: &Path) -> PlannedCommand {
        PlannedCommand::new(vec![
            uv.display().to_string(),
            "pip".to_string(),
            "install".to_string(),
            "--python".to_string(),
            self.venv_python(ctx).display().to_string(),
            "--reinstall-package".to_string(),
            "prisma".to_string(),
            format!("prisma=={}", crate::PRISMA_PINNED_VERSION),
        ])
    }

    /// Bring the venv's prisma-client-py to the pinned version
    /// ([`crate::PRISMA_PINNED_VERSION`]) before any generation:
    /// LiteLLM's PrismaWrapper is incompatible with prisma >= 0.13,
    /// and the standalone connect probe cannot detect that (the
    /// wrapper failure fires inside LiteLLM, after a standalone
    /// connect succeeds), so the version itself is the check.
    ///
    /// On a mismatch the generated client is cleared FIRST: the
    /// files `prisma generate` writes into the package directory
    /// are not tracked by the installer, so they survive reinstalls
    /// and version changes, and the stale client then crashes
    /// imports with errors from the wrong version (field evidence
    /// 2026-10-16: a downgrade left the 0.15-generated client in
    /// place and importing prisma died with a misleading
    /// `AttributeError: module 'prisma._types' has no attribute
    /// 'SortMode'`). Removing the whole package directory and
    /// reinstalling the pinned package leaves a clean wheel state;
    /// the caller's generate step then produces a fresh client.
    async fn ensure_prisma_pin(
        &self,
        ctx: &WizardContext,
        uv: &Path,
        log: &mut Vec<String>,
    ) -> Result<(), StepOutcome> {
        let (installed, probe_detail) = self.installed_prisma_version_probe(ctx).await;
        if installed.as_deref() == Some(crate::PRISMA_PINNED_VERSION) {
            return Ok(());
        }
        push_log(
            ctx,
            log,
            format!(
                "installed prisma-client-py ({}) does not match the pinned {} — clearing the stale generated client and reinstalling the pinned package",
                match (&installed, &probe_detail) {
                    (Some(v), _) => v.clone(),
                    (None, Some(detail)) => format!("unknown — probe failed: {detail}"),
                    (None, None) => "unknown".to_string(),
                },
                crate::PRISMA_PINNED_VERSION
            ),
        );
        let dir = self.prisma_package_dir(ctx);
        if ctx.ops.file_exists(&dir).await {
            ctx.ops.remove_dir_all(&dir).await.map_err(|e| {
                StepOutcome::failed(
                    format!(
                        "could not remove the stale Prisma package directory {}: {e}",
                        dir.display()
                    ),
                    log.clone(),
                )
            })?;
        }
        let cmd = self.prisma_pin_command(ctx, uv);
        const PIN_INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);
        match run_command_logged(ctx, log, &cmd, Some(PIN_INSTALL_TIMEOUT)).await {
            Ok(out) if out.success() => {
                push_log(
                    ctx,
                    log,
                    format!("prisma-client-py {} installed", crate::PRISMA_PINNED_VERSION),
                );
                Ok(())
            }
            Ok(out) => {
                let output = if out.stderr.trim().is_empty() {
                    out.stdout.trim()
                } else {
                    out.stderr.trim()
                };
                Err(StepOutcome::failed(
                    format!(
                        "installing the pinned prisma-client-py {} failed (exit {}): {}",
                        crate::PRISMA_PINNED_VERSION,
                        out.exit_code,
                        output
                    ),
                    log.clone(),
                ))
            }
            Err(e) => Err(StepOutcome::failed(
                format!(
                    "installing the pinned prisma-client-py {} could not run: {e}",
                    crate::PRISMA_PINNED_VERSION
                ),
                log.clone(),
            )),
        }
    }

    /// The generated client's entry point inside the prisma
    /// package: `prisma generate` writes `client.py` into the
    /// package directory, and the bare wheel does not contain one —
    /// its absence is exactly what makes the package raise "The
    /// Client hasn't been generated yet" (verified against the
    /// pinned 0.11.0 wheel: `prisma/__init__.py` imports
    /// `.client`, which only generation creates). Presence of this
    /// file is therefore the generation check — no process spawn
    /// needed.
    fn generated_client_path(&self, ctx: &WizardContext) -> PathBuf {
        self.prisma_package_dir(ctx).join("client.py")
    }

    /// Durable record of an in-progress Prisma repair:
    /// `<data_dir>/gateway/prisma-repair.json`. Written BEFORE the
    /// repair's first mutation and updated per phase; removed only
    /// when the repair verifies complete. Its presence at the start
    /// of a run means the previous repair was interrupted (app
    /// closed, step timeout) and is being resumed — the failure
    /// mode behind the 2026-10-16 half-repaired venv: the pin
    /// reinstall had deleted the generated client, the generate
    /// never ran, and nothing on disk said so.
    fn prisma_repair_state_path(ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("gateway").join("prisma-repair.json")
    }

    async fn write_prisma_repair_state(
        &self,
        ctx: &WizardContext,
        phase: &str,
        log: &mut Vec<String>,
    ) {
        let body = serde_json::json!({
            "pin": crate::PRISMA_PINNED_VERSION,
            "phase": phase,
            "at": chrono::Utc::now().to_rfc3339(),
        });
        if let Err(e) = ctx
            .ops
            .write_file(
                &Self::prisma_repair_state_path(ctx),
                json_pretty(&body).as_bytes(),
            )
            .await
        {
            // Best-effort: the repair itself still runs; only the
            // resume bookkeeping is lost.
            log.push(format!(
                "warning: could not record the Prisma repair state ({phase}): {e}"
            ));
        }
    }

    /// Ensure the venv's Prisma client is generated AND connects
    /// before the gateway spawns with DATABASE_URL. LiteLLM imports
    /// `prisma.Prisma` while starting; with no generated client the
    /// import raises RuntimeError ("The Client hasn't been
    /// generated yet") and the process dies in its lifespan — the
    /// exact failure this closes (found on real hardware
    /// 2026-10-16: the step's only Prisma touch was a `--version`
    /// pre-warm, which generates nothing). Without a database the
    /// gateway never imports Prisma, so this runs only on the
    /// DATABASE_URL path.
    ///
    /// VERIFY-FIRST: the overwhelmingly common start — the install
    /// was healthy last time — must cost seconds and mutate
    /// nothing. Verification is three checks: the installed version
    /// equals the pin (package metadata, no reinstall), the
    /// generated client is present ([`Self::generated_client_path`]),
    /// and the standalone connect probe passes. Only when a check
    /// fails does the repair machinery run: pin reinstall
    /// ([`Self::ensure_prisma_pin`]) → probe → generate → re-probe,
    /// bracketed by the durable repair state
    /// ([`Self::prisma_repair_state_path`]) so an interrupted repair
    /// is resumed by the next run instead of inherited half-done.
    /// A client that still does not connect standalone after a
    /// successful generate fails the step with the probe output
    /// instead of spawning a gateway that cannot use its database.
    async fn ensure_prisma_client_generated(
        &self,
        ctx: &WizardContext,
        uv: &Path,
        database_url: &str,
        log: &mut Vec<String>,
    ) -> Result<(), StepOutcome> {
        if ctx
            .ops
            .file_exists(&Self::prisma_repair_state_path(ctx))
            .await
        {
            log.push(
                "a previous Prisma repair did not complete (the app closed or the step timed out mid-repair) — resuming it"
                    .to_string(),
            );
        }
        // --- Verification (no mutation) ---
        let (installed, version_probe_detail) =
            self.installed_prisma_version_probe(ctx).await;
        let version_ok = installed.as_deref() == Some(crate::PRISMA_PINNED_VERSION);
        let client_present = ctx.ops.file_exists(&self.generated_client_path(ctx)).await;
        if version_ok && client_present {
            if self.prisma_probe_failure(ctx, database_url).await.is_none() {
                push_log(
                    ctx,
                    log,
                    "Prisma install verified (pinned version, generated client present, connect probe passed) — no changes made"
                        .to_string(),
                );
                // A verified install supersedes any interrupted
                // repair record.
                let _ = ctx
                    .ops
                    .remove_file(&Self::prisma_repair_state_path(ctx))
                    .await;
                return Ok(());
            }
            push_log(
                ctx,
                log,
                "Prisma verification failed at the connect probe — repairing the install"
                    .to_string(),
            );
        } else {
            push_log(
                ctx,
                log,
                format!(
                    "Prisma verification failed (installed version: {}, generated client: {}) — repairing the install",
                    match (&installed, &version_probe_detail) {
                        (Some(v), _) => v.clone(),
                        (None, Some(detail)) => format!("unknown — probe failed: {detail}"),
                        (None, None) => "unknown".to_string(),
                    },
                    if client_present { "present" } else { "missing" }
                ),
            );
        }
        // --- Repair (durable state first, then mutations) ---
        self.write_prisma_repair_state(ctx, "pin", log).await;
        self.ensure_prisma_pin(ctx, uv, log).await?;
        self.write_prisma_repair_state(ctx, "probe", log).await;
        if self.prisma_probe_failure(ctx, database_url).await.is_none() {
            log.push("Prisma client verified after the pin repair".to_string());
            let _ = ctx
                .ops
                .remove_file(&Self::prisma_repair_state_path(ctx))
                .await;
            return Ok(());
        }
        log.push(
            "Prisma client probe failed — generating the client before the gateway starts"
                .to_string(),
        );
        self.write_prisma_repair_state(ctx, "generate", log).await;
        let schema = self.resolve_prisma_schema(ctx, database_url, log).await?;
        log.push(format!(
            "generating the Prisma client from {}",
            schema.display()
        ));
        self.run_prisma_generate(ctx, database_url, &schema, log)
            .await?;
        self.write_prisma_repair_state(ctx, "verify", log).await;
        match self.prisma_probe_failure(ctx, database_url).await {
            None => {
                log.push("Prisma client verified after generation".to_string());
                let _ = ctx
                    .ops
                    .remove_file(&Self::prisma_repair_state_path(ctx))
                    .await;
                Ok(())
            }
            Some(detail) => Err(StepOutcome::failed(
                format!("Prisma client still does not connect after prisma generate: {detail}"),
                log.clone(),
            )),
        }
    }

    /// Post-start MCP load check (design §8, §11, §14.6): with the
    /// master key, ask the running gateway which MCP servers it
    /// registered (`GET /v1/mcp/server`) and list each armed
    /// server's tools (`GET /mcp-rest/tools/list?server_id=<id>`).
    /// A server passes when it appears in the registered list AND
    /// its tools list returns without error. Servers the gateway
    /// reports that are NOT in the generated set — possible
    /// DB-registered extras the manager does not own (§14.6) — are
    /// logged as a warning naming them. Returns the ids that
    /// failed; the caller logs and notes them but does not fail the
    /// step. No state is persisted: the MCP view re-queries the
    /// gateway live (Phase 4).
    async fn mcp_load_check(
        &self,
        ctx: &WizardContext,
        armed_ids: &[String],
        log: &mut Vec<String>,
    ) -> Vec<String> {
        let base = format!(
            "http://{}:{}",
            gateway_probe_host(&ctx.config),
            ctx.config.gateway.port
        );
        let headers = self.gateway_auth_headers(ctx).await;
        let mut registered: Vec<String> = Vec::new();
        match ctx
            .ops
            .http_get_with_headers(&format!("{base}/v1/mcp/server"), &headers)
            .await
        {
            Ok(resp) if resp.status == 200 => {
                registered = crate::parse_mcp_registered_server_ids(&resp.body_str());
            }
            Ok(resp) => log.push(format!(
                "MCP load check: GET /v1/mcp/server returned HTTP {} — cannot confirm which MCP servers the gateway registered",
                resp.status
            )),
            Err(e) => log.push(format!("MCP load check: GET /v1/mcp/server failed: {e}")),
        }
        for extra in registered.iter().filter(|id| !armed_ids.contains(id)) {
            log.push(format!(
                "warning: the gateway reports MCP server '{extra}' which is not in the generated configuration (registered outside the manager, e.g. in the LiteLLM database); it is not managed by this app"
            ));
        }
        let mut failed: Vec<String> = Vec::new();
        for id in armed_ids {
            let mut problems: Vec<String> = Vec::new();
            if !registered.iter().any(|r| r == id) {
                problems.push("not in the gateway's registered MCP server list".to_string());
            }
            let url = format!("{base}/mcp-rest/tools/list?server_id={id}");
            match ctx.ops.http_get_with_headers(&url, &headers).await {
                Ok(resp) if resp.status == 200 => {
                    if let Some(err) = crate::mcp_response_error(&resp.body_str()) {
                        problems.push(format!("tools list returned an error: {err}"));
                    }
                }
                Ok(resp) => {
                    let detail = crate::mcp_response_error(&resp.body_str())
                        .unwrap_or_else(|| format!("HTTP {}", resp.status));
                    problems.push(format!("tools list failed: {detail}"));
                }
                Err(e) => problems.push(format!("tools list request failed: {e}")),
            }
            if problems.is_empty() {
                log.push(format!(
                    "MCP load check passed for '{id}' (registered; tools list OK)"
                ));
            } else {
                log.push(format!(
                    "MCP load check failed for '{id}': {}",
                    problems.join("; ")
                ));
                failed.push(id.clone());
            }
        }
        failed
    }

    async fn wait_healthy(&self, ctx: &WizardContext, pid: u32, log: &mut Vec<String>) -> bool {
        let url = self.gateway_url(ctx);
        let headers = self.gateway_auth_headers(ctx).await;
        let wait_started = std::time::Instant::now();
        push_log(
            ctx,
            log,
            format!(
                "waiting for the gateway to answer on :{} (up to {} probes)",
                ctx.config.gateway.port, self.health_retries
            ),
        );
        for attempt in 1..=self.health_retries {
            // Fail fast when the child is gone: probing a dead process
            // for the full budget reports a timeout and hides the real
            // failure, which the captured log tail then has to explain.
            if !ctx.ops.process_alive(pid) {
                push_log(
                    ctx,
                    log,
                    format!(
                        "gateway process (pid {pid}) exited during startup (probe attempt {attempt})"
                    ),
                );
                return false;
            }
            // Probe outcomes are recorded on the first attempt,
            // every tenth, and on success: a full 120-attempt wait
            // otherwise floods the step log with near-identical
            // lines while showing nothing new.
            let log_attempt = attempt == 1 || attempt.is_multiple_of(10);
            match ctx.ops.http_get_with_headers(&url, &headers).await {
                Ok(resp) if resp.status == 200 => {
                    push_log(
                        ctx,
                        log,
                        format!(
                            "gateway healthy on :{} (attempt {attempt})",
                            ctx.config.gateway.port
                        ),
                    );
                    return true;
                }
                Ok(resp) => {
                    if log_attempt {
                        push_log(
                            ctx,
                            log,
                            format!(
                                "health probe attempt {attempt}: HTTP {} ({:.0}s elapsed)",
                                resp.status,
                                wait_started.elapsed().as_secs_f64()
                            ),
                        );
                    }
                }
                Err(e) => {
                    if log_attempt {
                        push_log(
                            ctx,
                            log,
                            format!(
                                "health probe attempt {attempt}: {e} ({:.0}s elapsed)",
                                wait_started.elapsed().as_secs_f64()
                            ),
                        );
                    }
                }
            }
            if attempt < self.health_retries {
                tokio::time::sleep(std::time::Duration::from_millis(self.health_interval_ms)).await;
            }
        }
        false
    }

    /// Sustained-health confirmation for the DATABASE_URL path:
    /// after the first healthy probe, keep probing for
    /// `db_sustain_ms` and require the gateway to stay alive and
    /// answering the whole window, then close with the standalone
    /// Prisma connect probe. Both known gateway-with-database
    /// defects strike about a second AFTER a plain connect (and a
    /// first health probe) succeeds — the Windows watchdog kill
    /// takes the whole process down (the window catches that), and
    /// the PrismaWrapper connect exception leaves the database dead
    /// while the process keeps serving (only the closing standalone
    /// probe catches that — HTTP health cannot). A zero window
    /// disables the confirmation (tests).
    async fn confirm_sustained_health(
        &self,
        ctx: &WizardContext,
        pid: u32,
        database_url: &str,
        log: &mut Vec<String>,
    ) -> bool {
        if self.db_sustain_ms == 0 {
            return true;
        }
        push_log(
            ctx,
            log,
            format!(
                "confirming the gateway stays healthy for {} s (database path)",
                self.db_sustain_ms / 1000
            ),
        );
        let url = self.gateway_url(ctx);
        let headers = self.gateway_auth_headers(ctx).await;
        let window = std::time::Duration::from_millis(self.db_sustain_ms);
        let deadline = std::time::Instant::now() + window;
        let mut successes = 0u32;
        loop {
            if !ctx.ops.process_alive(pid) {
                log.push(format!(
                    "gateway process (pid {pid}) exited during the database-path confirmation window"
                ));
                return false;
            }
            match ctx.ops.http_get_with_headers(&url, &headers).await {
                Ok(resp) if resp.status == 200 => {
                    successes += 1;
                }
                Ok(resp) => {
                    log.push(format!(
                        "gateway stopped answering during the database-path confirmation window: HTTP {}",
                        resp.status
                    ));
                    return false;
                }
                Err(e) => {
                    log.push(format!(
                        "gateway stopped answering during the database-path confirmation window: {e}"
                    ));
                    return false;
                }
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            let step = std::time::Duration::from_millis(self.health_interval_ms.max(1));
            tokio::time::sleep(step.min(remaining)).await;
        }
        // A single lucky 200 at the window's start proves nothing
        // about the span: require at least two successful probes
        // across it.
        if successes < 2 {
            log.push(format!(
                "gateway answered only {successes} health probe(s) during the {} s confirmation window — not a sustained confirmation",
                window.as_secs()
            ));
            return false;
        }
        // Close with the standalone probe: the gateway process can
        // be serving while its database client is dead (the
        // PrismaWrapper connect failure), which no HTTP health
        // probe distinguishes.
        if let Some(detail) = self.prisma_probe_failure(ctx, database_url).await {
            log.push(format!(
                "standalone Prisma probe failed at the end of the confirmation window: {detail} — the gateway is serving but its database client is not connected"
            ));
            return false;
        }
        log.push(format!(
            "gateway still healthy after the {} s database-path confirmation window ({} probes, standalone Prisma probe passed)",
            window.as_secs(),
            successes
        ));
        true
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
            // bootstrap download when uv is not yet resolvable). The
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
                // MCP gateway servers (design §4.2): planned parity
                // with the spawned env, following the OpenRouter
                // precedent exactly — decided from the config alone
                // (a plan performs no credential reads), placeholders
                // never values. The stdio flag is planned when any
                // Gateway-exposed stdio server is configured; if its
                // credential cannot be read at run time the server
                // is not armed and the spawned env omits its entries.
                let gateway_mcp: Vec<&manager_config::McpServerConfig> = ctx
                    .config
                    .mcp
                    .servers
                    .iter()
                    .filter(|s| s.exposure == manager_config::McpExposure::Gateway)
                    .collect();
                if gateway_mcp
                    .iter()
                    .any(|s| s.transport == manager_config::McpTransport::Stdio)
                {
                    spawn.env.insert(
                        "LITELLM_ENABLE_MCP_STDIO".to_string(),
                        "true".to_string(),
                    );
                }
                for server in &gateway_mcp {
                    if server.cred_ref.as_deref().is_some_and(|r| !r.is_empty()) {
                        for key in &server.env_keys {
                            spawn.env.insert(
                                key.clone(),
                                "<set from credential manager>".to_string(),
                            );
                        }
                    }
                }
                // SearXNG (design §6.3): planned parity with the
                // spawned env. The api_base is not a secret — it is
                // config — so the plan shows the real value, the way
                // spawn_command's own non-secret env values
                // (PYTHONIOENCODING et al.) are shown.
                let searxng = &ctx.config.mcp.searxng;
                if searxng.enabled && !searxng.api_base.trim().is_empty() {
                    spawn.env.insert(
                        "SEARXNG_API_BASE".to_string(),
                        searxng.api_base.trim().to_string(),
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
                // Built once, up front: every failure path below
                // attaches it, so a failed start always shows how
                // to start the gateway by hand.
                let manual = self.manual_start_block(ctx).await;
                let uv = match self.ensure_uv(ctx).await {
                    Ok(p) => {
                        push_log(ctx, &mut log, format!("using uv at {}", p.display()));
                        p
                    }
                    Err(outcome) => return Ok(outcome.with_manual_command(manual)),
                };
                // Reuse a healthy existing venv: one already on the
                // pinned Python with litellm importable needs no venv
                // creation and no package install. A venv on the
                // pinned Python whose packages are missing or broken
                // needs only the package install — recreating the
                // venv (`uv venv --clear`) for a package-level
                // problem threw away a sound interpreter and redid
                // minutes of work on every retry. Only a venv on the
                // WRONG Python (or no venv at all) is (re)created;
                // --clear replaces a venv left behind by a failed
                // install or built with an unpinned (too new)
                // Python, which previously made every retry fail
                // with "already exists" before the real error.
                // The probes carry their failure evidence into the
                // log (see venv_python_version_probe): which repair
                // runs — and why — is always stated before it runs.
                let (existing_version, version_probe_detail) =
                    self.venv_python_version_probe(ctx).await;
                let venv_current =
                    existing_version.as_deref() == Some(crate::GATEWAY_PYTHON_VERSION);
                let (litellm_ok, import_probe_detail) = if venv_current {
                    self.litellm_import_probe(ctx).await
                } else {
                    (false, None)
                };
                let setup_cmds: Vec<PlannedCommand> = if litellm_ok {
                    push_log(
                        ctx,
                        &mut log,
                        format!(
                            "existing install verified — skipping install (venv on Python {}; litellm, prisma and prometheus_client import cleanly)",
                            crate::GATEWAY_PYTHON_VERSION
                        ),
                    );
                    Vec::new()
                } else if venv_current {
                    push_log(
                        ctx,
                        &mut log,
                        format!(
                            "venv is on Python {} but the litellm import probe failed ({}) — installing packages only (the venv itself is kept)",
                            crate::GATEWAY_PYTHON_VERSION,
                            import_probe_detail.as_deref().unwrap_or("no detail")
                        ),
                    );
                    vec![self.pip_command(ctx, &uv)]
                } else {
                    match &existing_version {
                        Some(v) => push_log(
                            ctx,
                            &mut log,
                            format!(
                                "existing venv uses Python {v}; recreating it with Python {}",
                                crate::GATEWAY_PYTHON_VERSION
                            ),
                        ),
                        None => {
                            if ctx.ops.file_exists(&self.venv_python(ctx)).await {
                                push_log(
                                    ctx,
                                    &mut log,
                                    format!(
                                        "the venv interpreter probe failed ({}) — recreating the venv with Python {}",
                                        version_probe_detail.as_deref().unwrap_or("no detail"),
                                        crate::GATEWAY_PYTHON_VERSION
                                    ),
                                );
                            } else {
                                push_log(
                                    ctx,
                                    &mut log,
                                    format!(
                                        "creating venv with Python {}",
                                        crate::GATEWAY_PYTHON_VERSION
                                    ),
                                );
                            }
                        }
                    }
                    vec![self.venv_command(ctx, &uv), self.pip_command(ctx, &uv)]
                };
                // Package installs get a longer command budget than
                // the ops default: a cold `uv pip install` of
                // litellm[proxy] + prisma on a slow link is the one
                // legitimate multi-minute command in the wizard.
                const SETUP_COMMAND_TIMEOUT: std::time::Duration =
                    std::time::Duration::from_secs(900);
                for cmd in &setup_cmds {
                    match run_command_logged(ctx, &mut log, cmd, Some(SETUP_COMMAND_TIMEOUT))
                        .await
                    {
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
                                log,
                            )
                            .with_manual_command(manual))
                        }
                        Err(e) => {
                            return Ok(
                                StepOutcome::failed(format!("{e}"), log)
                                    .with_manual_command(manual),
                            )
                        }
                    }
                }
                // Probe the installed LiteLLM version before the
                // spawn (MCP design §4.3): the result is recorded in
                // the spawn state below and drives the MCP floor
                // check in the arming pass that follows.
                let litellm_version = self.installed_litellm_version(ctx).await;
                match &litellm_version {
                    Some(v) => push_log(ctx, &mut log, format!("detected LiteLLM version: {v}")),
                    None => push_log(
                        ctx,
                        &mut log,
                        "LiteLLM version could not be determined (the version probe returned nothing usable)"
                            .to_string(),
                    ),
                }
                // MCP gateway arming (design §4.2), resolved BEFORE
                // the YAML is generated: the generated config, the
                // spawn environment and the spawn fingerprint all key
                // off this one armed set (the single rule lives in
                // crate::armed_gateway_mcp_servers). Each Gateway
                // server's credential is read once here; a server
                // whose credential cannot be read is NOT armed — it
                // is omitted from this start with a warning and the
                // gateway start proceeds: a missing MCP credential
                // never fails the gateway.
                let mut mcp_cred_values: std::collections::HashMap<String, String> =
                    std::collections::HashMap::new();
                for server in ctx.config.mcp.servers.iter().filter(|s| {
                    s.exposure == manager_config::McpExposure::Gateway && !s.env_keys.is_empty()
                }) {
                    let Some(cred_ref) = server.cred_ref.as_deref().filter(|r| !r.is_empty())
                    else {
                        log.push(format!(
                            "warning: MCP server '{}' is not armed: it declares env keys but no credential reference — the server is omitted from this gateway start",
                            server.id
                        ));
                        continue;
                    };
                    if mcp_cred_values.contains_key(cred_ref) {
                        continue;
                    }
                    match ctx
                        .ops
                        .credential_read(cred_ref)
                        .await
                        .filter(|v| !v.is_empty())
                    {
                        Some(value) => {
                            mcp_cred_values.insert(cred_ref.to_string(), value);
                        }
                        None => log.push(format!(
                            "warning: MCP server '{}' is not armed: credential '{cred_ref}' could not be read from Windows Credential Manager — the server is omitted from this gateway start",
                            server.id
                        )),
                    }
                }
                let mcp_armed: Vec<&manager_config::McpServerConfig> =
                    crate::armed_gateway_mcp_servers(&ctx.config, |t| {
                        mcp_cred_values.contains_key(t)
                    });
                let mcp_stdio_armed = crate::mcp_armed_set_has_stdio(&mcp_armed);
                // Floor enforcement (design §4.3), fail-closed BEFORE
                // any spawn: with an armed stdio Gateway server, the
                // installed LiteLLM must be at or above
                // LITELLM_MCP_FLOOR_VERSION (the release carrying the
                // stdio command/args allowlist fix). An UNDETERMINED
                // version also fails: the allowlist state would be
                // unverifiable, and the CVE surface is exactly that
                // allowlist — a deliberate conservative reading.
                if mcp_stdio_armed {
                    let meets_floor = litellm_version
                        .as_deref()
                        .and_then(crate::litellm_version_meets_mcp_floor);
                    if meets_floor != Some(true) {
                        return Ok(StepOutcome::failed(
                            format!(
                                "MCP stdio gateway servers require LiteLLM >= {} (the stdio allowlist fix), but the installed version is {}. Remediation: set updates.components[\"litellm\"].pinned_version to a version at or above {} (or clear a stale pin) and re-run this step",
                                crate::LITELLM_MCP_FLOOR_VERSION,
                                litellm_version
                                    .as_deref()
                                    .unwrap_or("unknown — the version probe could not determine it"),
                                crate::LITELLM_MCP_FLOOR_VERSION
                            ),
                            log,
                        )
                        .with_manual_command(manual));
                    }
                }
                // config.yaml is generated from app state, never hand-edited.
                // Both failure paths carry the manual block and the
                // accumulated step log, like every other failure in
                // this step — they previously returned a one-line
                // debug outcome with neither (code-review 2.8).
                // Generate from the armed-only view of the config:
                // unarmed Gateway servers are omitted from this
                // start's YAML (the generator itself stays a pure
                // function of the config it is handed).
                let generation_config = crate::config_with_armed_gateway_mcp_servers(
                    &ctx.config,
                    |t| mcp_cred_values.contains_key(t),
                );
                let yaml = match manager_gateway::generate_config_yaml(&generation_config) {
                    Ok(y) => y,
                    Err(e) => {
                        return Ok(StepOutcome::failed(
                            format!("gateway config generation failed: {e}"),
                            log,
                        )
                        .with_manual_command(manual))
                    }
                };
                let cfg_path = ctx.data_dir.join("gateway").join("config.yaml");
                if let Err(e) = ctx.ops.write_file(&cfg_path, yaml.as_bytes()).await {
                    return Ok(StepOutcome::failed(
                        format!(
                            "could not write the gateway configuration {}: {e}",
                            cfg_path.display()
                        ),
                        log,
                    )
                    .with_manual_command(manual));
                }
                log.push(format!("wrote {}", cfg_path.display()));
                // The context-overflow routing callback
                // (routing-rules design §6.3) ships as a generated
                // sibling the config's `callbacks:` entry resolves
                // config-directory-relative. Written when a typed
                // rule is enabled; a stale copy is removed when not,
                // so disabling the rule restores exactly the
                // pre-feature gateway directory.
                let routing_path = ctx.data_dir.join("gateway").join("llmdil_routing.py");
                match manager_gateway::generate_routing_callback(&generation_config) {
                    Some(callback_py) => {
                        if let Err(e) =
                            ctx.ops.write_file(&routing_path, callback_py.as_bytes()).await
                        {
                            return Ok(StepOutcome::failed(
                                format!(
                                    "could not write the gateway routing callback {}: {e}",
                                    routing_path.display()
                                ),
                                log,
                            )
                            .with_manual_command(manual));
                        }
                        log.push(format!("wrote {}", routing_path.display()));
                    }
                    None => {
                        if let Err(e) = ctx.ops.remove_file(&routing_path).await {
                            return Ok(StepOutcome::failed(
                                format!(
                                    "could not remove the stale gateway routing callback {}: {e}",
                                    routing_path.display()
                                ),
                                log,
                            )
                            .with_manual_command(manual));
                        }
                    }
                }
                // Restart = stop the recorded gateway first; without
                // this the spawn below is a duplicate that fails to
                // bind while the old process keeps serving.
                if let Err(outcome) = self.stop_previous_gateway(ctx, &mut log).await {
                    return Ok(outcome.with_manual_command(manual));
                }
                // App-managed PostgreSQL: when install-postgres has
                // run, the server must be up before the gateway
                // spawns — the gateway receives DATABASE_URL and
                // migrates against it at startup. Not installed →
                // no DATABASE_URL, the pre-PostgreSQL behavior.
                let database_url = if postgres_installed(ctx.ops.as_ref(), &ctx.data_dir).await {
                    match ensure_postgres_running(ctx, &mut log).await {
                        Ok(url) => {
                            // Verify (and only if needed, repair)
                            // the Prisma install before the spawn:
                            // the gateway imports the Prisma client
                            // at startup and dies if the install is
                            // broken, so that work happens here,
                            // where its failure is reported as this
                            // step's failure — not as a dead gateway.
                            // On a healthy install this is three
                            // checks and no mutation (see
                            // ensure_prisma_client_generated); the
                            // old `--version` pre-warm is gone — it
                            // generated nothing, proved nothing the
                            // connect probe does not prove better,
                            // and cost a process spawn on every
                            // start.
                            if let Err(outcome) = self
                                .ensure_prisma_client_generated(ctx, &uv, &url, &mut log)
                                .await
                            {
                                return Ok(outcome.with_manual_command(manual));
                            }
                            Some(url)
                        }
                        Err(e) => {
                            return Ok(StepOutcome::failed(e, log).with_manual_command(manual))
                        }
                    }
                } else {
                    None
                };
                // The spawn env's MCP entries: each armed server's
                // credential value (read once in the arming pass)
                // under each of its env-key names.
                let mcp_env: Vec<(String, String)> = mcp_armed
                    .iter()
                    .flat_map(|s| {
                        let value = s
                            .cred_ref
                            .as_deref()
                            .and_then(|r| mcp_cred_values.get(r))
                            .cloned();
                        s.env_keys.iter().filter_map(move |k| {
                            value.clone().map(|v| (k.clone(), v))
                        })
                    })
                    .collect();
                let (mut spawn, openrouter_warning) = match self
                    .spawn_command_with_env(ctx, database_url.as_deref(), &mcp_env, mcp_stdio_armed)
                    .await
                {
                    Ok(pair) => pair,
                    Err(outcome) => return Ok(outcome.with_manual_command(manual)),
                };
                if let Some(w) = &openrouter_warning {
                    log.push(format!("warning: {w}"));
                }
                // Capture the gateway's own output: without it a
                // startup failure is undiagnosable from inside the app.
                spawn.stdout_log = Some(self.gateway_stdout_log(ctx));
                spawn.stderr_log = Some(self.gateway_stderr_log(ctx));
                push_log(
                    ctx,
                    &mut log,
                    format!(
                        "gateway output is captured to {} and {}",
                        self.gateway_stdout_log(ctx).display(),
                        self.gateway_stderr_log(ctx).display()
                    ),
                );
                push_log(
                    ctx,
                    &mut log,
                    format!(
                        "spawning the gateway process ({})",
                        spawn.argv.first().cloned().unwrap_or_default()
                    ),
                );
                let pid = if ctx.config.supervision.mode == SupervisionMode::Service {
                    // Service mode: regenerate the gateway's service
                    // definition from this spawn (secret values become
                    // credential references — never written to the
                    // definition file) and start the service. The
                    // health checks below run unchanged against the
                    // child pid the service host recorded.
                    match self
                        .spawn_gateway_as_service(ctx, &spawn, &mut log)
                        .await
                    {
                        Ok(pid) => pid,
                        Err(outcome) => {
                            return Ok(outcome.with_manual_command(manual))
                        }
                    }
                } else {
                    match ctx.ops.spawn_detached(&spawn).await {
                        Ok(pid) => pid,
                        Err(e) => {
                            return Ok(StepOutcome::failed(
                                format!("{e}"),
                                vec![format!("{e:?}")],
                            )
                            .with_manual_command(manual))
                        }
                    }
                };
                push_log(ctx, &mut log, format!("LiteLLM spawned (pid {pid})"));
                // Record the pid so a later stop (the gateway_stop
                // command) or restart (this step re-run) terminates
                // exactly this process instead of guessing by port.
                let pid_path = crate::gateway_pid_path(&ctx.data_dir);
                match ctx
                    .ops
                    .write_file(&pid_path, pid.to_string().as_bytes())
                    .await
                {
                    Ok(()) => push_log(
                        ctx,
                        &mut log,
                        format!("recorded gateway pid in {}", pid_path.display()),
                    ),
                    Err(e) => log.push(format!(
                        "warning: could not record the gateway pid in {}: {e}; \
                         stopping this gateway later will not be possible",
                        pid_path.display()
                    )),
                }
                // First-healthy is not enough on the database path:
                // the known database defects kill the gateway about
                // a second after it first answers, so that path also
                // has to survive the confirmation window.
                let health_failure = if !self.wait_healthy(ctx, pid, &mut log).await {
                    Some("did not become healthy")
                } else if let Some(url) = &database_url {
                    if self.confirm_sustained_health(ctx, pid, url, &mut log).await {
                        None
                    } else {
                        Some("became healthy but did not stay up")
                    }
                } else {
                    None
                };
                if let Some(health_failure) = health_failure {
                    // The gateway's own output is the diagnosis —
                    // appended to the step log above its tail,
                    // LABELED as gateway output. The failure MESSAGE
                    // is the step's own verdict only: embedding the
                    // raw stderr tail in the message presented it as
                    // the step's error, and unrelated gateway log
                    // noise (auth rejections from the app's own
                    // status polling) reached the operator labeled
                    // as the cause of the failure (2026-10-16).
                    self.append_gateway_log_tail(ctx, &mut log).await;
                    return Ok(StepOutcome::failed(
                        format!(
                            "gateway {health_failure} on :{} — the gateway's output is in the step log and in {}",
                            ctx.config.gateway.port,
                            ctx.data_dir.join("logs").display()
                        ),
                        log,
                    )
                    .with_manual_command(manual));
                }
                // Post-start MCP load check (design §8, §11, §14.6):
                // every armed server must appear in the gateway's
                // registered list and answer its tools list. A
                // failure is logged with the gateway's own error
                // text and noted in the outcome message, but never
                // fails the step — the gateway itself is healthy.
                // Nothing is persisted: the MCP view re-queries the
                // gateway live (Phase 4).
                let mcp_load_failed: Vec<String> = if mcp_armed.is_empty() {
                    Vec::new()
                } else {
                    let armed_ids: Vec<String> =
                        mcp_armed.iter().map(|s| s.id.clone()).collect();
                    self.mcp_load_check(ctx, &armed_ids, &mut log).await
                };
                // Record what this gateway was actually spawned
                // with (config fingerprint + the YAML): the drift
                // check at the next launch compares the record
                // against the then-current configuration, YAML and
                // spawn-only inputs alike (code-review 2.3).
                {
                    let fingerprint = crate::gateway_spawn_fingerprint(
                        &ctx.config,
                        database_url.is_some(),
                        spawn.env.get("LITELLM_MASTER_KEY").map(String::as_str),
                        spawn.env.get("OPENROUTER_API_KEY").map(String::as_str),
                        &mcp_armed,
                    );
                    let state_json = crate::gateway_spawn_state_json(
                        &fingerprint,
                        &yaml,
                        litellm_version.as_deref(),
                    );
                    match ctx
                        .ops
                        .write_file(
                            &crate::gateway_spawn_state_path(&ctx.data_dir),
                            state_json.as_bytes(),
                        )
                        .await
                    {
                        Ok(()) => log.push(
                            "recorded the gateway spawn state for the next launch's drift check"
                                .to_string(),
                        ),
                        Err(e) => log.push(format!(
                            "warning: could not record the gateway spawn state: {e}; \
                             the next launch will treat this start as drifted and re-run this step"
                        )),
                    }
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
                if !mcp_load_failed.is_empty() {
                    message = format!(
                        "{message} — warning: MCP load check failed for: {}",
                        mcp_load_failed.join(", ")
                    );
                }
                // Final re-check, immediately before reporting
                // done: between the health confirmation above and
                // here, the step runs the MCP load check and the
                // firewall reconcile — the reconcile can wait on a
                // UAC prompt for minutes — and a gateway that died
                // in that window was still reported as a successful
                // start (field report 2026-10-19: the step said
                // "done" while the Gateway page showed the gateway
                // down). One last probe at the health check's own
                // standard: in process mode the spawned pid must
                // still be alive (a service host may legitimately
                // restart its child, so service mode relies on the
                // probe alone) and the authenticated models
                // endpoint must answer 200. A failure here fails
                // the step with the gateway's output attached, like
                // any other health failure.
                let final_check_failure =
                    if ctx.config.supervision.mode != SupervisionMode::Service
                        && !ctx.ops.process_alive(pid)
                    {
                        Some(format!(
                            "the gateway process (pid {pid}) exited after its health checks passed"
                        ))
                    } else {
                        let headers = self.gateway_auth_headers(ctx).await;
                        match ctx
                            .ops
                            .http_get_with_headers(&self.gateway_url(ctx), &headers)
                            .await
                        {
                            Ok(resp) if resp.status == 200 => None,
                            Ok(resp) => Some(format!(
                                "the gateway answered HTTP {} on the final health re-check after passing its health checks",
                                resp.status
                            )),
                            Err(e) => Some(format!(
                                "the gateway is unreachable on the final health re-check ({e}) after passing its health checks"
                            )),
                        }
                    };
                if let Some(reason) = final_check_failure {
                    self.append_gateway_log_tail(ctx, &mut log).await;
                    return Ok(StepOutcome::failed(
                        format!(
                            "gateway started but did not stay up: {reason} — the gateway's output is in the step log and in {}",
                            ctx.data_dir.join("logs").display()
                        ),
                        log,
                    )
                    .with_manual_command(manual));
                }
                push_log(
                    ctx,
                    &mut log,
                    "final health re-check passed — the gateway is still serving".to_string(),
                );
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

/// Step 8: verify MCP server credentials exist.
///
/// MCP Phase 1 (design §10.1): this step no longer writes
/// `<data_dir>/mcp/mcp-servers.json`. The persisted registry in
/// `AppConfig.mcp` is the source of truth now; the old file is
/// retired as a store, left in place if present (deleting user files
/// is not the migration's job), and noted as inert in the step log.
///
/// MCP Phase 4 (design §9): the presence check covers EVERY
/// registry server whose exposure is not `Disabled` and that names
/// a `cred_ref` — not only the two legacy servers. Presence only —
/// values are never read. A consequence, by design: a fresh wizard
/// run now fails this step while the seeded `brave_search` server
/// (Gateway-exposed, credential target
/// `local-llm-service-manager/brave-search-mcp`) has no stored
/// credential, until the operator stores one or sets the server
/// Disabled.
pub struct McpServers;

impl McpServers {
    /// Path of the retired `mcp-servers.json` store (design §10.1).
    /// Used only to note its presence as inert in the step log —
    /// nothing reads or writes it any more.
    fn retired_config_path(&self, ctx: &WizardContext) -> PathBuf {
        ctx.data_dir.join("mcp").join("mcp-servers.json")
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
        Some("store the missing credential (MCP Servers view) or set the server Disabled, then re-run mcp-servers")
    }
    async fn run(&self, ctx: &WizardContext) -> Result<StepOutcome, WizardError> {
        let mut log = Vec::new();
        // Credential presence check (design §9): every registry
        // server that is not Disabled and names a cred_ref. Values
        // are never read or shown — presence only. All missing
        // servers are collected (and logged) before failing, so one
        // run names every gap instead of only the first.
        let checked: Vec<&manager_config::McpServerConfig> = ctx
            .config
            .mcp
            .servers
            .iter()
            .filter(|s| {
                s.exposure != McpExposure::Disabled
                    && s.cred_ref
                        .as_deref()
                        .map(str::trim)
                        .map(|r| !r.is_empty())
                        .unwrap_or(false)
            })
            .collect();
        let mut missing: Vec<(&str, &str)> = Vec::new();
        for server in &checked {
            let target = server.cred_ref.as_deref().unwrap_or_default();
            match ctx.ops.credential_exists(target).await {
                Ok(true) => log.push(format!(
                    "mcp server '{}': credential '{target}' present",
                    server.id
                )),
                Ok(false) => {
                    log.push(format!(
                        "mcp server '{}': credential '{target}' missing",
                        server.id
                    ));
                    missing.push((server.id.as_str(), target));
                }
                Err(e) => return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")])),
            }
        }
        if !missing.is_empty() {
            let named = missing
                .iter()
                .map(|(id, target)| format!("{id} ('{target}')"))
                .collect::<Vec<_>>()
                .join(", ");
            // The per-server present/missing log lines ride the
            // failure's log tail, ahead of the remediation.
            log.push(format!(
                "missing MCP server credential(s): {named} — store the credential (MCP Servers view), or set the server Disabled, then retry this step"
            ));
            log.push("credential values are never displayed or stored".to_string());
            return Ok(StepOutcome::failed(
                format!(
                    "MCP server credential(s) not found in Windows Credential Manager: {named}"
                ),
                log,
            ));
        }
        // Retired store (design §10.1): the step writes nothing. If a
        // pre-Phase-1 mcp-servers.json is still on disk, say plainly
        // in the success log that it is inert — left in place, and
        // nothing reads it.
        let retired = self.retired_config_path(ctx);
        if ctx.ops.file_exists(&retired).await {
            log.push(format!(
                "note: {} is present but inert — the retired mcp-servers.json store is no longer written or read; the persisted AppConfig.mcp registry is the source of truth",
                retired.display()
            ));
        } else {
            log.push(
                "note: the retired mcp-servers.json store is no longer written; the persisted AppConfig.mcp registry is the source of truth"
                    .to_string(),
            );
        }
        let checked_ids = checked
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Ok(StepOutcome::done(
            format!("MCP server credentials present ({checked_ids})"),
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
                push_log(ctx, &mut log, format!("downloading {url}"));
                let progress = ProgressEmitter::new(ctx, pin.tarball_asset.clone());
                let dl_started = std::time::Instant::now();
                match ctx
                    .ops
                    .download(&url, &tarball, &|d, t| progress.progress(d, t))
                    .await
                {
                    Ok(bytes) => push_log(
                        ctx,
                        &mut log,
                        format!("downloaded {bytes} bytes in {}", fmt_elapsed(dl_started)),
                    ),
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
            push_log(
                ctx,
                &mut log,
                format!(
                    "extracting {} to {}",
                    pin.tarball_asset,
                    mxc_bin_dir(ctx).display()
                ),
            );
            let extract_started = std::time::Instant::now();
            if let Err(e) = ctx
                .ops
                .extract_tar_gz(&tarball, &root, "package/bin/x64/", "package/")
                .await
            {
                return Ok(StepOutcome::failed(format!("{e}"), vec![format!("{e:?}")]));
            }
            push_log(
                ctx,
                &mut log,
                format!(
                    "extracted package/bin/x64 to {} in {}",
                    mxc_bin_dir(ctx).display(),
                    fmt_elapsed(extract_started)
                ),
            );
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
            // A model load can take most of the budget; the wait
            // announces itself and reports in about every 30 s on
            // the live sink instead of sitting silent.
            push_log(
                ctx,
                log,
                format!(
                    "waiting for backend '{id}' on :{port} to become healthy (model load; up to {} s)",
                    self.ready_budget_ms / 1000
                ),
            );
            let mut last_tick_s = 0u64;
            loop {
                if let Ok(resp) = ctx.ops.http_get(&url).await {
                    if resp.status == 200 {
                        push_log(
                            ctx,
                            log,
                            format!(
                                "backend '{id}' healthy on :{port} after {}",
                                fmt_elapsed(start)
                            ),
                        );
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
                let elapsed_s = start.elapsed().as_secs();
                if elapsed_s >= last_tick_s + 30 {
                    last_tick_s = elapsed_s;
                    ctx.emit_log(&format!(
                        "still waiting for backend '{id}' on :{port} — {elapsed_s}s elapsed"
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
    use crate::{CommandOutput, GpuDescriptor, RunMode, StepState, WizardContext};
    use manager_config::{AppConfig, ComponentUpdate, ModelConfig};
    use sha2::{Digest, Sha256};
    use std::sync::Arc;

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    #[test]
    fn gateway_launch_spec_maps_mcp_credentials_to_references() {
        // Code-review B-7: an armed gateway MCP server's credential
        // must reach the service definition as a credential
        // reference, never as a plaintext literal; non-secret env
        // and local-exposure servers' keys stay literal.
        let mut config = AppConfig::default_config();
        let brave_target = config
            .mcp
            .servers
            .iter()
            .find(|s| s.id == "brave_search")
            .and_then(|s| s.cred_ref.clone())
            .expect("curated brave server names a credential");
        config.mcp.servers.push(manager_config::McpServerConfig {
            id: "local_only".to_string(),
            env_keys: vec!["LOCAL_ONLY_KEY".to_string()],
            cred_ref: Some("local-llm-service-manager/local-only".to_string()),
            exposure: manager_config::McpExposure::Local,
            ..Default::default()
        });
        let mut cmd = PlannedCommand::new(vec!["pythonw.exe".to_string()]);
        cmd.env
            .insert("BRAVE_API_KEY".to_string(), "sentinel-mcp-value".to_string());
        cmd.env
            .insert("LOCAL_ONLY_KEY".to_string(), "local-value".to_string());
        cmd.env
            .insert("PLAIN_SETTING".to_string(), "plain".to_string());
        let spec = gateway_launch_spec(&cmd, &config);
        let brave = spec
            .env
            .iter()
            .find(|e| e.name == "BRAVE_API_KEY")
            .expect("brave env entry present");
        assert_eq!(
            brave.value,
            manager_services::EnvValue::CredentialRef { target: brave_target }
        );
        let local = spec
            .env
            .iter()
            .find(|e| e.name == "LOCAL_ONLY_KEY")
            .expect("local env entry present");
        assert_eq!(
            local.value,
            manager_services::EnvValue::Literal {
                value: "local-value".to_string()
            }
        );
        let serialized = serde_json::to_string(&spec.env).unwrap();
        assert!(
            !serialized.contains("sentinel-mcp-value"),
            "MCP credential value must not appear in the definition env: {serialized}"
        );
        assert!(serialized.contains("plain"));
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
                verified_sha256: None,
                source_url: "https://example.com/qwen3-8b.gguf".to_string(),
                assigned_backends: vec!["tool-runner".to_string()],
                enabled: true,
                display_name: None,
                moe_layers: None,
                default_offload_layers: None,
                mtp: None,
            },
            ModelConfig {
                id: "qwen2.5-coder-7b".to_string(),
                gguf_path: "models/qwen2.5-coder-7b.gguf".into(),
                quant: "Q4_K_M".to_string(),
                params_b: 7.0,
                sha256: sha256_hex(b"gguf-7b-bytes"),
                verified_at: None,
                verified_sha256: None,
                source_url: "https://example.com/qwen2.5-coder-7b.gguf".to_string(),
                assigned_backends: vec!["coder-fast".to_string()],
                enabled: true,
                display_name: None,
                moe_layers: None,
                default_offload_layers: None,
                mtp: None,
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
        // This test pins the split step's contract for backends that
        // need a split. Since the memory-requirement work (v0.1.31),
        // an unsplit backend is valid when its GPU-resident weights
        // fit GPU 0 or it is served by expert offload — so
        // the fixture neutralizes both escape hatches: no offload
        // flags, and the shared model carries the coder catalog id
        // (49 GB — fits no single test GPU).
        for b in config.backends.iter_mut() {
            b.raw_flags = None;
        }
        for m in config.models.iter_mut() {
            if m.gguf_path.ends_with("qwen3-8b.gguf") {
                m.id = "qwen3-coder-30b".to_string();
            }
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

    // --- DownloadModels: audit verified-by-stamp skip ---

    fn catalog_size(id: &str) -> u64 {
        manager_models::curated_models()
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.size_bytes)
            .unwrap()
    }

    /// An audit ctx whose only enabled model is qwen3-8b, with the
    /// given stamp state and scripted on-disk length. The fake file
    /// holds bytes whose hash does NOT match the pin, so an audit
    /// pass proves the hash was skipped and a failure proves it ran.
    fn audit_ctx_stamped(
        ops: &Arc<FakeSystemOps>,
        stamped: bool,
        scripted_len: Option<u64>,
    ) -> WizardContext {
        let mut config = test_config_with_models();
        config.models[1].enabled = false;
        if stamped {
            config.models[0].verified_at = Some(chrono::Utc::now());
            // The stamp is bound to the pin it verified against
            // (code-review 2.4): without this the skip cannot fire.
            config.models[0].verified_sha256 = Some(config.models[0].sha256.clone());
        }
        let dest = resolve_dest(&PathBuf::from("/data"), &config.models[0].gguf_path);
        ops.with_file(dest.clone(), b"not-the-pinned-bytes".to_vec());
        if let Some(len) = scripted_len {
            ops.with_file_len(dest, len);
        }
        WizardContext::new(config, PathBuf::from("/data"), RunMode::Audit, ops.clone())
    }

    #[tokio::test]
    async fn download_models_audit_skips_hash_for_stamped_model_with_matching_length() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = audit_ctx_stamped(&ops, true, Some(catalog_size("qwen3-8b")));
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("verified by an earlier checksum pass")),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn download_models_audit_hashes_when_length_differs_or_stamp_missing() {
        // Length differs from the catalog pin: the hash runs (and
        // fails — the fake bytes are not the pinned content).
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = audit_ctx_stamped(&ops, true, Some(catalog_size("qwen3-8b") - 1));
        let out = DownloadModels.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(out.message.contains("checksum mismatch"), "{}", out.message);

        // No stamp: the hash runs even at the exact catalog length.
        let ops2 = Arc::new(FakeSystemOps::new());
        let ctx2 = audit_ctx_stamped(&ops2, false, Some(catalog_size("qwen3-8b")));
        let out2 = DownloadModels.run(&ctx2).await.unwrap();
        assert_eq!(out2.state, StepState::Failed, "{out2:?}");
        assert!(
            out2.message.contains("checksum mismatch"),
            "{}",
            out2.message
        );

        // Stamp against a DIFFERENT pin than the current one (the
        // catalog was re-pinned after the stamp): the skip must not
        // fire — the hash runs and fails (code-review 2.4).
        let ops3 = Arc::new(FakeSystemOps::new());
        let mut ctx3 = audit_ctx_stamped(&ops3, true, Some(catalog_size("qwen3-8b")));
        ctx3.config.models[0].verified_sha256 = Some("0".repeat(64));
        let out3 = DownloadModels.run(&ctx3).await.unwrap();
        assert_eq!(out3.state, StepState::Failed, "{out3:?}");
        assert!(
            out3.message.contains("checksum mismatch"),
            "{}",
            out3.message
        );
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
            db_sustain_ms: 0,
        };
        let planned = step.planned_commands(&ctx);
        assert_eq!(planned.len(), 7);
        // Probes first, then the pinned venv creation, pip, the spawn,
        // and the firewall reconciliation (show rule, elevated script).
        assert!(planned[2].argv[0].ends_with(StartGateway::uv_binary_name()));
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
            db_sustain_ms: 0,
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
            db_sustain_ms: 0,
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

    /// A recording live sink for the logging tests: every line a
    /// step emits while running lands here.
    fn recording_sink() -> (crate::StepLogSink, Arc<std::sync::Mutex<Vec<String>>>) {
        let lines: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = lines.clone();
        (
            Arc::new(move |line: String| captured.lock().unwrap().push(line)),
            lines,
        )
    }

    #[tokio::test]
    async fn start_gateway_fast_path_logs_existing_install_verified() {
        // The healthy-start contract (field report 2026-10-19):
        // the step log states the verification and the skip, with
        // the probe evidence — and the same lines reach the live
        // sink WHILE the step runs (the wizard log is written from
        // the sink, so this is what the operator watches).
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let (sink, emitted) = recording_sink();
        let ctx = ctx_with(ops.clone(), RunMode::Install).with_log_sink(sink);
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
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("existing install verified — skipping install")),
            "{:?}",
            out.log_tail
        );
        let emitted = emitted.lock().unwrap();
        assert!(
            emitted
                .iter()
                .any(|l| l.contains("existing install verified — skipping install")),
            "live sink received the verification line: {emitted:?}"
        );
        assert!(
            emitted
                .iter()
                .any(|l| l.contains("waiting for the gateway to answer")),
            "live sink received the health-wait line: {emitted:?}"
        );
        assert!(
            emitted
                .iter()
                .any(|l| l.contains("final health re-check passed")),
            "live sink received the final re-check line: {emitted:?}"
        );
    }

    #[tokio::test]
    async fn start_gateway_import_probe_failure_logs_error_and_repairs_packages_only() {
        // The reinstall-on-every-start defect: a probe failure used
        // to be a bare bool, so the log never said WHAT failed and
        // the repair looked identical to a full reinstall. The
        // import error itself must be logged BEFORE the repair,
        // and a package-level failure must not recreate the venv.
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        ops.with_file(step.venv_python(&ctx), Vec::new());
        let version_probe = step.version_probe_command(&ctx);
        let version_argv: Vec<&str> = version_probe.argv.iter().map(String::as_str).collect();
        ops.with_command(
            &version_argv,
            crate::CommandOutput {
                exit_code: 0,
                stdout: format!("{}\n", crate::GATEWAY_PYTHON_VERSION),
                stderr: String::new(),
            },
        );
        let import_probe = step.litellm_probe_command(&ctx);
        let import_argv: Vec<&str> = import_probe.argv.iter().map(String::as_str).collect();
        ops.with_command(
            &import_argv,
            crate::CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: "ModuleNotFoundError: No module named 'litellm'".to_string(),
            },
        );
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let repair_line = out
            .log_tail
            .iter()
            .position(|l| l.contains("litellm import probe failed"))
            .expect("the probe failure is logged");
        assert!(
            out.log_tail[repair_line].contains("ModuleNotFoundError"),
            "{}",
            out.log_tail[repair_line]
        );
        assert!(
            out.log_tail[repair_line].contains("installing packages only"),
            "{}",
            out.log_tail[repair_line]
        );
        // The repair line precedes the pip run's own log line.
        let pip_line = out
            .log_tail
            .iter()
            .position(|l| l.starts_with("running:") && l.contains("pip"))
            .expect("the pip repair is logged");
        assert!(repair_line < pip_line, "{:?}", out.log_tail);
        let executed = ops.executed_commands();
        assert!(
            executed.iter().any(|a| a.iter().any(|x| x == "install")),
            "pip install ran: {executed:?}"
        );
        assert!(
            executed.iter().all(|a| !a.iter().any(|x| x == "venv")),
            "the venv itself is kept: {executed:?}"
        );
    }

    #[tokio::test]
    async fn start_gateway_tolerates_noisy_version_probe_output() {
        // Output pollution around the version line (an interpreter
        // warning, a sitecustomize banner) used to fail the
        // whole-stdout equality parse and force a full venv
        // rebuild on every start. Noise is not a different
        // version.
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        ops.with_file(step.venv_python(&ctx), Vec::new());
        let probe = step.version_probe_command(&ctx);
        let probe_argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
        ops.with_command(
            &probe_argv,
            crate::CommandOutput {
                exit_code: 0,
                stdout: format!(
                    "Warning: interpreter plugin failed to load\n{}\n",
                    crate::GATEWAY_PYTHON_VERSION
                ),
                stderr: String::new(),
            },
        );
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("existing install verified — skipping install")),
            "{:?}",
            out.log_tail
        );
        let executed = ops.executed_commands();
        assert!(
            executed
                .iter()
                .all(|a| !a.iter().any(|x| x == "venv" || x == "install")),
            "no venv/pip commands may run: {executed:?}"
        );
    }

    #[tokio::test]
    async fn start_gateway_fails_when_gateway_dies_after_health_checks() {
        // The false-"done" hole (field report 2026-10-19): health
        // passed, then the gateway died during the post-health
        // work (MCP check, firewall reconcile) — the step still
        // reported done. The final re-check must catch it.
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get_sequence(
            "http://127.0.0.1:4000/v1/models",
            vec![
                FakeSystemOps::http_ok(r#"{"data":[]}"#),
                crate::HttpResponse {
                    status: 500,
                    body: Vec::new(),
                    elapsed_ms: 0,
                },
            ],
        );
        ops.with_file(
            PathBuf::from("/data/logs/gateway.stderr.log"),
            b"litellm: fatal error after startup\n".to_vec(),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(
            out.message.contains("did not stay up"),
            "{}",
            out.message
        );
        assert!(
            out.message.contains("HTTP 500"),
            "{}",
            out.message
        );
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("fatal error after startup")),
            "gateway output attached: {:?}",
            out.log_tail
        );
    }

    #[test]
    fn version_parsers_tolerate_noise_and_reject_other_shapes() {
        assert_eq!(parse_two_part_version("3.12\n").as_deref(), Some("3.12"));
        assert_eq!(
            parse_two_part_version("Warning: noisy plugin\n3.12\n").as_deref(),
            Some("3.12")
        );
        assert_eq!(parse_two_part_version("3.12.1\n"), None);
        assert_eq!(parse_two_part_version("Python 3.12\n"), None);
        assert_eq!(parse_two_part_version(""), None);
        assert_eq!(
            parse_semver_version("1.105.1\n").as_deref(),
            Some("1.105.1")
        );
        assert_eq!(
            parse_semver_version("UserWarning: something\n0.11.0\n").as_deref(),
            Some("0.11.0")
        );
        assert_eq!(parse_semver_version("1.2\n"), None);
        assert_eq!(parse_semver_version("1.2.3.4\n"), None);
        assert_eq!(parse_semver_version("no version here\n"), None);
    }

    #[test]
    fn progress_emitter_throttles_ticks() {
        let ops = Arc::new(FakeSystemOps::new());
        let (sink, emitted) = recording_sink();
        let ctx = ctx_with(ops, RunMode::Install).with_log_sink(sink);
        let emitter = ProgressEmitter::new(&ctx, "thing.zip");
        emitter.progress(0, 100); // first call always emits
        emitter.progress(5, 100); // <10-point step, <5 s: throttled
        emitter.progress(15, 100); // >=10-point step: emits
        emitter.progress(100, 100); // completion: emits
        let emitted = emitted.lock().unwrap();
        assert_eq!(emitted.len(), 3, "{emitted:?}");
        assert!(emitted[0].contains("(0%)"), "{}", emitted[0]);
        assert!(emitted[1].contains("(15%)"), "{}", emitted[1]);
        assert!(emitted[2].contains("(100%)"), "{}", emitted[2]);
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
            db_sustain_ms: 0,
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
            db_sustain_ms: 0,
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
            db_sustain_ms: 0,
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
            db_sustain_ms: 0,
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
            db_sustain_ms: 0,
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
            db_sustain_ms: 0,
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
            db_sustain_ms: 0,
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
            db_sustain_ms: 0,
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
            db_sustain_ms: 0,
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
            db_sustain_ms: 0,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        assert!(out.message.contains("did not become healthy"));
    }

    #[tokio::test]
    async fn start_gateway_failure_carries_manual_start_block() {
        let _guard = PathGuard::set_to(&path_dir_with_uv(true));
        let ops = Arc::new(FakeSystemOps::new());
        // No scripted health response → probes fail.
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let step = StartGateway {
            health_retries: 2,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        let manual = out
            .manual_command
            .expect("a failed start carries the manual start block");
        // The spawn command exactly as the step builds it: the
        // bootstrap entry point, this install's config path, port.
        assert!(manual.contains("run_server"), "{manual}");
        assert!(manual.contains("--config"), "{manual}");
        assert!(manual.contains("config.yaml"), "{manual}");
        assert!(manual.contains("4000"), "{manual}");
        // The spawn line opens with the PowerShell call operator —
        // without it a quoted executable path is a parse error.
        let spawn_line = manual
            .lines()
            .find(|l| l.contains("run_server"))
            .expect("manual block carries the spawn line");
        assert!(spawn_line.starts_with("& "), "{manual}");
        // The master key is named with its Credential Manager
        // target, and the stored value never appears in the block.
        assert!(manual.contains("LITELLM_MASTER_KEY"), "{manual}");
        assert!(
            manual.contains(crate::LITELLM_MASTER_KEY_CRED_TARGET),
            "{manual}"
        );
        let stored = ops
            .credential_read(crate::LITELLM_MASTER_KEY_CRED_TARGET)
            .await
            .expect("the step stored a master key");
        assert!(
            !manual.contains(&stored),
            "manual block leaked the master key value"
        );
        // PostgreSQL is not installed in the fake fs: no pg_ctl
        // pre-step and no DATABASE_URL line.
        assert!(!manual.contains("pg_ctl"), "{manual}");
        assert!(!manual.contains("DATABASE_URL"), "{manual}");
        assert!(
            !manual.contains("PRISMA_HEALTH_WATCHDOG_ENABLED"),
            "{manual}"
        );
    }

    #[tokio::test]
    async fn manual_start_block_includes_postgres_pre_step_when_installed() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        ops.with_file(
            postgres_bin_dir(&ctx.data_dir).join("pg_ctl.exe"),
            Vec::new(),
        );
        ops.with_file(postgres_marker_path(&ctx.data_dir), b"{}".to_vec());
        let block = StartGateway::default().manual_start_block(&ctx).await;
        assert!(block.contains("pg_ctl"), "{block}");
        assert!(
            block.contains(&format!(
                "postgresql://postgres:<password>@127.0.0.1:{}/litellm",
                ctx.config.postgres.port
            )),
            "{block}"
        );
        assert!(!block.contains("<port>"), "no placeholder port: {block}");
        assert!(
            block.contains(crate::POSTGRES_PASSWORD_CRED_TARGET),
            "{block}"
        );
        // The watchdog disable rides with the DATABASE_URL line:
        // a manual start with a database needs it on Windows or
        // the gateway dies about a second after startup.
        let db_at = block.find("DATABASE_URL").unwrap();
        let watchdog_at = block
            .find("PRISMA_HEALTH_WATCHDOG_ENABLED")
            .expect("manual block carries the watchdog disable");
        assert!(db_at < watchdog_at, "{block}");
        // The pg_ctl pre-step precedes the gateway spawn line.
        let pg_at = block.find("pg_ctl").unwrap();
        let spawn_at = block.find("run_server").unwrap();
        assert!(pg_at < spawn_at, "{block}");
        assert!(watchdog_at < spawn_at, "{block}");
        // Both executable lines open with the PowerShell call
        // operator; the $env: lines between them do not.
        let pg_line = block
            .lines()
            .find(|l| l.contains("pg_ctl"))
            .expect("manual block carries the pg_ctl line");
        assert!(pg_line.starts_with("& "), "{block}");
        let spawn_line = block
            .lines()
            .find(|l| l.contains("run_server"))
            .expect("manual block carries the spawn line");
        assert!(spawn_line.starts_with("& "), "{block}");
    }

    /// Plant the files that make `postgres_installed` true in the
    /// fake fs (binaries in place + the install marker).
    fn with_postgres_installed(ops: &FakeSystemOps, ctx: &WizardContext) {
        ops.with_file(
            postgres_bin_dir(&ctx.data_dir).join("pg_ctl.exe"),
            Vec::new(),
        );
        ops.with_file(postgres_marker_path(&ctx.data_dir), b"{}".to_vec());
    }

    /// The schema path the Prisma tests script the resolution probe
    /// to report (and plant in the fake fs).
    fn prisma_test_schema(ctx: &WizardContext) -> PathBuf {
        ctx.data_dir
            .join("litellm")
            .join("proxy")
            .join("schema.prisma")
    }

    const PRISMA_SCHEMA_SCRIPT: &str = "import litellm, pathlib; print(pathlib.Path(litellm.__file__).parent / 'proxy' / 'schema.prisma')";

    /// The connect probe's argv as the step builds it (the
    /// DATABASE_URL lives in the command env, not the argv, so a
    /// placeholder URL yields the identical argv to script).
    fn prisma_probe_argv(step: &StartGateway, ctx: &WizardContext) -> Vec<String> {
        step.prisma_client_probe_command(ctx, "postgresql://probe")
            .argv
    }

    fn prisma_not_generated_output() -> CommandOutput {
        CommandOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "RuntimeError: The Client hasn't been generated yet, you must run `prisma generate` before you can use the client.".to_string(),
        }
    }

    /// (a) PostgreSQL installed, the client was never generated:
    /// the probe fails, the step generates, the re-probe passes,
    /// and the gateway spawns.
    #[tokio::test]
    async fn start_gateway_generates_prisma_client_when_missing() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        with_postgres_installed(&ops, &ctx);
        let py = step.venv_python(&ctx).display().to_string();
        let schema = prisma_test_schema(&ctx);
        ops.with_file(schema.clone(), b"// schema".to_vec());
        // Connect probe: fails before generation, passes after.
        let probe_argv = prisma_probe_argv(&step, &ctx);
        let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
        ops.with_command_sequence(
            &probe_refs,
            vec![prisma_not_generated_output(), FakeSystemOps::ok_output("")],
        );
        ops.with_command(
            &[py.as_str(), "-c", PRISMA_SCHEMA_SCRIPT],
            FakeSystemOps::ok_output(&format!("{}\n", schema.display())),
        );
        // `prisma generate` itself stays unscripted: default success.
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let executed = ops.executed_cmds();
        // Generation ran exactly once, against the resolved schema.
        let generates: Vec<&PlannedCommand> = executed
            .iter()
            .filter(|c| c.argv.iter().any(|a| a == "generate"))
            .collect();
        assert_eq!(generates.len(), 1, "{executed:?}");
        assert!(generates[0]
            .argv
            .iter()
            .any(|a| a == &schema.display().to_string()));
        // ... with DATABASE_URL set and the venv's script directory
        // first on PATH (the Prisma CLI spawns prisma-client-py by
        // name from there; without it generate fails ENOENT).
        let env = &generates[0].env;
        assert!(env.contains_key("DATABASE_URL"), "{env:?}");
        let path_env = env
            .get("PATH")
            .expect("the generate command carries a PATH");
        assert!(
            path_env.starts_with(&step.venv_scripts_dir(&ctx).display().to_string()),
            "{path_env}"
        );
        // The connect probe ran twice: before and after generation.
        let probes = executed.iter().filter(|c| c.argv == probe_argv).count();
        assert_eq!(probes, 2, "{executed:?}");
        // Each stage is in the step log.
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("Prisma client generated")),
            "{out:?}"
        );
        // And the gateway spawned with DATABASE_URL.
        assert!(ops.spawn_envs()[0].contains_key("DATABASE_URL"));
    }

    /// (b) Generation fails: the step fails, the generate output is
    /// in the message, the manual start block is attached, and
    /// nothing spawns.
    #[tokio::test]
    async fn start_gateway_fails_when_prisma_generate_fails() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        with_postgres_installed(&ops, &ctx);
        let py = step.venv_python(&ctx).display().to_string();
        let schema = prisma_test_schema(&ctx);
        let schema_str = schema.display().to_string();
        ops.with_file(schema.clone(), b"// schema".to_vec());
        let probe_argv = prisma_probe_argv(&step, &ctx);
        let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
        ops.with_command(&probe_refs, prisma_not_generated_output());
        ops.with_command(
            &[py.as_str(), "-c", PRISMA_SCHEMA_SCRIPT],
            FakeSystemOps::ok_output(&format!("{schema_str}\n")),
        );
        ops.with_command(
            &[
                py.as_str(),
                "-m",
                "prisma",
                "generate",
                "--schema",
                schema_str.as_str(),
            ],
            CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: "Error: spawn prisma-client-py ENOENT".to_string(),
            },
        );
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(out.message.contains("prisma generate failed"), "{out:?}");
        assert!(
            out.message.contains("spawn prisma-client-py ENOENT"),
            "{out:?}"
        );
        assert!(out.manual_command.is_some(), "{out:?}");
        assert!(ops.spawns().is_empty(), "no spawn after failed generation");
    }

    /// (c) No PostgreSQL: the DATABASE_URL path never runs — no
    /// probe, no schema resolution, no generate, and the spawn
    /// carries no DATABASE_URL.
    #[tokio::test]
    async fn start_gateway_skips_prisma_generation_without_postgres() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let executed = ops.executed_commands();
        assert!(
            !executed.iter().any(|argv| argv.iter().any(|a| {
                a.contains("from prisma import Prisma")
                    || a == "generate"
                    || a.contains("schema.prisma")
            })),
            "{executed:?}"
        );
        assert!(!ops.spawn_envs()[0].contains_key("DATABASE_URL"));
    }

    /// (d) Client already generated: the probe passes and no
    /// generation is issued — the fast path for every later start.
    #[tokio::test]
    async fn start_gateway_skips_generation_when_client_already_generated() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        with_postgres_installed(&ops, &ctx);
        let probe_argv = prisma_probe_argv(&step, &ctx);
        let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
        ops.with_command(&probe_refs, FakeSystemOps::ok_output(""));
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let executed = ops.executed_commands();
        assert!(
            !executed
                .iter()
                .any(|argv| argv.iter().any(|a| a == "generate")),
            "{executed:?}"
        );
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("Prisma client verified")),
            "{out:?}"
        );
    }

    /// (f) Generation reports success but the client still fails
    /// its standalone connect probe. The step fails with the probe
    /// output and the manual block instead of spawning a gateway
    /// that cannot use its database.
    #[tokio::test]
    async fn start_gateway_fails_when_client_still_broken_after_generate() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        with_postgres_installed(&ops, &ctx);
        let py = step.venv_python(&ctx).display().to_string();
        let schema = prisma_test_schema(&ctx);
        ops.with_file(schema.clone(), b"// schema".to_vec());
        ops.with_command(
            &[py.as_str(), "-c", PRISMA_SCHEMA_SCRIPT],
            FakeSystemOps::ok_output(&format!("{}\n", schema.display())),
        );
        // Connect probe: not generated, then still failing after
        // generation.
        let probe_argv = prisma_probe_argv(&step, &ctx);
        let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
        ops.with_command_sequence(
            &probe_refs,
            vec![
                prisma_not_generated_output(),
                CommandOutput {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "prisma engine exited before the connection was established"
                        .to_string(),
                },
            ],
        );
        // `prisma generate` itself stays unscripted: default success.
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        let message = out.message.clone();
        assert!(message.contains("engine exited"), "{message}");
        assert!(message.contains("still does not connect"), "{message}");
        assert!(out.manual_command.is_some(), "{out:?}");
        assert!(ops.spawns().is_empty());
    }

    /// The watchdog disable is set exactly on the Windows +
    /// DATABASE_URL combination: the os.kill defect is
    /// Windows-only, and the watchdog only runs with a database.
    #[test]
    fn prisma_watchdog_flag_decision() {
        let url = Some("postgresql://postgres@127.0.0.1:54329/litellm");
        assert!(StartGateway::prisma_watchdog_must_be_disabled(true, url));
        assert!(!StartGateway::prisma_watchdog_must_be_disabled(true, None));
        assert!(!StartGateway::prisma_watchdog_must_be_disabled(false, url));
        assert!(!StartGateway::prisma_watchdog_must_be_disabled(false, None));
    }

    /// The version probe's argv as the step builds it.
    fn prisma_version_argv(step: &StartGateway, ctx: &WizardContext) -> Vec<String> {
        step.prisma_version_command(ctx).argv.clone()
    }

    /// The LiteLLM version probe's argv as the step builds it.
    fn litellm_version_argv(step: &StartGateway, ctx: &WizardContext) -> Vec<String> {
        step.litellm_version_command(ctx).argv.clone()
    }

    #[tokio::test]
    async fn start_gateway_records_probed_litellm_version() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        // The probe mirrors the prisma pair: venv python + -c +
        // importlib.metadata for the 'litellm' distribution.
        let version_argv = litellm_version_argv(&step, &ctx);
        assert_eq!(version_argv.len(), 3);
        assert_eq!(version_argv[1], "-c");
        assert!(
            version_argv[2].contains("importlib.metadata")
                && version_argv[2].contains("version('litellm')"),
            "{version_argv:?}"
        );
        let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
        ops.with_command(&version_refs, FakeSystemOps::ok_output("1.105.1\n"));
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("detected LiteLLM version: 1.105.1")),
            "{out:?}"
        );
        // The probed version lands in the recorded spawn state.
        let state = ops
            .file_bytes(&crate::gateway_spawn_state_path(&ctx.data_dir))
            .expect("spawn state recorded");
        let state: serde_json::Value = serde_json::from_slice(&state).unwrap();
        assert_eq!(state["litellm_version"], "1.105.1");
    }

    #[tokio::test]
    async fn start_gateway_logs_undetermined_litellm_version() {
        // No scripted probe output: the fake's unscripted command
        // succeeds with empty output, which the probe reads as
        // undetermined — the step still succeeds and the log says
        // the version could not be determined (never a guess).
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("LiteLLM version could not be determined")),
            "{out:?}"
        );
        let state = ops
            .file_bytes(&crate::gateway_spawn_state_path(&ctx.data_dir))
            .expect("spawn state recorded");
        let state: serde_json::Value = serde_json::from_slice(&state).unwrap();
        assert_eq!(state["litellm_version"], serde_json::Value::Null);
    }

    // --- StartGateway: MCP gateway servers (Phase 2) ---

    fn mcp_stdio_gateway_server() -> manager_config::McpServerConfig {
        manager_config::McpServerConfig {
            id: "brave_search".to_string(),
            display_name: "Brave Search".to_string(),
            transport: manager_config::McpTransport::Stdio,
            exposure: manager_config::McpExposure::Gateway,
            command: Some("npx".to_string()),
            args: vec![
                "-y".to_string(),
                "@brave/brave-search-mcp-server".to_string(),
            ],
            url: None,
            env_keys: vec!["BRAVE_API_KEY".to_string()],
            cred_ref: Some("local-llm-service-manager/brave-search-mcp".to_string()),
            signup_url: None,
            credential_hint: None,
            allowed_tools: None,
            metered: false,
            rpm_limit: None,
        }
    }

    fn gateway_ctx_with_mcp(
        ops: Arc<FakeSystemOps>,
        servers: Vec<manager_config::McpServerConfig>,
    ) -> WizardContext {
        let mut cfg = test_config_with_models();
        cfg.mcp.servers = servers;
        WizardContext::new(cfg, PathBuf::from("/data"), RunMode::Install, ops)
    }

    fn script_litellm_version(
        ops: &FakeSystemOps,
        step: &StartGateway,
        ctx: &WizardContext,
        version: &str,
    ) {
        let argv = litellm_version_argv(step, ctx);
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        ops.with_command(&refs, FakeSystemOps::ok_output(&format!("{version}\n")));
    }

    fn fast_step() -> StartGateway {
        StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        }
    }

    #[tokio::test]
    async fn start_gateway_arms_stdio_mcp_server_with_credential() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_credential_value("local-llm-service-manager/brave-search-mcp", "brave-secret");
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/mcp/server",
            FakeSystemOps::http_ok(r#"[{"server_id":"brave_search"}]"#),
        );
        ops.with_http_get(
            "http://127.0.0.1:4000/mcp-rest/tools/list?server_id=brave_search",
            FakeSystemOps::http_ok(r#"{"tools":[{"name":"brave_web_search"}]}"#),
        );
        let step = fast_step();
        let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
        script_litellm_version(&ops, &step, &ctx, "1.105.1");
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        // Spawn env: stdio flag + the credential under its env key.
        let envs = ops.spawn_envs();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].get("LITELLM_ENABLE_MCP_STDIO").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            envs[0].get("BRAVE_API_KEY").map(String::as_str),
            Some("brave-secret")
        );
        // YAML registers the server with an env REFERENCE, never the value.
        let cfg = ops
            .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
            .expect("config written");
        let text = String::from_utf8(cfg).unwrap();
        assert!(text.contains("mcp_servers:"), "{text}");
        assert!(text.contains("brave_search"), "{text}");
        assert!(text.contains("require_key_mcp_access_defined: true"), "{text}");
        assert!(!text.contains("brave-secret"), "{text}");
        // Load check passed and is logged.
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("MCP load check passed for 'brave_search'")),
            "{out:?}"
        );
        // Fingerprint records the armed set.
        let state = ops
            .file_bytes(&crate::gateway_spawn_state_path(&ctx.data_dir))
            .expect("spawn state recorded");
        let state: serde_json::Value = serde_json::from_slice(&state).unwrap();
        assert_eq!(state["fingerprint"]["mcp_stdio_enabled"], true);
        assert!(
            !state["fingerprint"]["mcp_servers_hash"]
                .as_str()
                .unwrap()
                .is_empty(),
            "{state}"
        );
    }

    #[tokio::test]
    async fn start_gateway_unarmed_mcp_server_is_omitted_and_gateway_still_starts() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        // No credential scripted: the arming read fails.
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = fast_step();
        let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
        // No LiteLLM version scripted either: with no armed stdio
        // server the floor check must not fire.
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("'brave_search' is not armed")),
            "{out:?}"
        );
        let envs = ops.spawn_envs();
        assert_eq!(envs.len(), 1);
        assert!(!envs[0].contains_key("LITELLM_ENABLE_MCP_STDIO"));
        assert!(!envs[0].contains_key("BRAVE_API_KEY"));
        let cfg = ops
            .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
            .expect("config written");
        let text = String::from_utf8(cfg).unwrap();
        assert!(!text.contains("mcp_servers"), "{text}");
        assert!(!text.contains("brave_search"), "{text}");
        let state = ops
            .file_bytes(&crate::gateway_spawn_state_path(&ctx.data_dir))
            .expect("spawn state recorded");
        let state: serde_json::Value = serde_json::from_slice(&state).unwrap();
        assert_eq!(state["fingerprint"]["mcp_stdio_enabled"], false);
        assert_eq!(state["fingerprint"]["mcp_servers_hash"], "");
    }

    #[tokio::test]
    async fn start_gateway_fails_closed_below_mcp_floor_with_armed_stdio() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_credential_value("local-llm-service-manager/brave-search-mcp", "brave-secret");
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = fast_step();
        let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
        script_litellm_version(&ops, &step, &ctx, "1.80.0");
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(out.message.contains("1.83.7"), "{}", out.message);
        assert!(out.message.contains("pinned_version"), "{}", out.message);
        assert!(ops.spawns().is_empty(), "floor failure precedes any spawn");
    }

    #[tokio::test]
    async fn start_gateway_fails_closed_when_version_undetermined_with_armed_stdio() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_credential_value("local-llm-service-manager/brave-search-mcp", "brave-secret");
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = fast_step();
        let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
        // Version probe unscripted → undetermined → fail-closed.
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(out.message.contains("unknown"), "{}", out.message);
        assert!(ops.spawns().is_empty());
    }

    #[tokio::test]
    async fn start_gateway_mcp_load_check_failure_is_noted_not_fatal() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_credential_value("local-llm-service-manager/brave-search-mcp", "brave-secret");
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        // The gateway registered nothing, and the tools list errors.
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/mcp/server",
            FakeSystemOps::http_ok("[]"),
        );
        ops.with_http_get(
            "http://127.0.0.1:4000/mcp-rest/tools/list?server_id=brave_search",
            crate::HttpResponse {
                status: 500,
                body: br#"{"error":{"message":"server failed to start"}}"#.to_vec(),
                elapsed_ms: 1,
            },
        );
        let step = fast_step();
        let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
        script_litellm_version(&ops, &step, &ctx, "1.105.1");
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.message.contains("MCP load check failed for: brave_search"),
            "{out:?}"
        );
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("server failed to start")),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn start_gateway_mcp_load_check_warns_on_unmanaged_registered_servers() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_credential_value("local-llm-service-manager/brave-search-mcp", "brave-secret");
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/mcp/server",
            FakeSystemOps::http_ok(
                r#"[{"server_id":"brave_search"},{"server_id":"db_extra"}]"#,
            ),
        );
        ops.with_http_get(
            "http://127.0.0.1:4000/mcp-rest/tools/list?server_id=brave_search",
            FakeSystemOps::http_ok(r#"{"tools":[{"name":"brave_web_search"}]}"#),
        );
        let step = fast_step();
        let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
        script_litellm_version(&ops, &step, &ctx, "1.105.1");
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("'db_extra'") && l.contains("not in the generated configuration")),
            "{out:?}"
        );
        assert!(!out.message.contains("MCP load check failed"), "{out:?}");
    }

    #[test]
    fn start_gateway_planned_spawn_carries_mcp_env_placeholders() {
        let ops = Arc::new(FakeSystemOps::new());
        let step = fast_step();
        let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
        let planned = step.planned_commands(&ctx);
        let plan_spawn = planned
            .iter()
            .find(|c| c.argv.iter().any(|a| a == "--config"))
            .expect("gateway spawn planned");
        assert_eq!(
            plan_spawn.env.get("LITELLM_ENABLE_MCP_STDIO").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            plan_spawn.env.get("BRAVE_API_KEY").map(String::as_str),
            Some("<set from credential manager>")
        );
    }

    fn gateway_ctx_with_searxng(
        ops: Arc<FakeSystemOps>,
        api_base: &str,
    ) -> WizardContext {
        let mut cfg = test_config_with_models();
        cfg.mcp.servers = vec![];
        cfg.mcp.searxng = manager_config::SearxngSettings {
            enabled: true,
            api_base: api_base.to_string(),
        };
        WizardContext::new(cfg, PathBuf::from("/data"), RunMode::Install, ops)
    }

    #[tokio::test]
    async fn start_gateway_searxng_env_yaml_and_fingerprint() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = fast_step();
        let ctx = gateway_ctx_with_searxng(ops.clone(), "http://127.0.0.1:8080");
        script_litellm_version(&ops, &step, &ctx, "1.105.1");
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        // Spawn env carries the real base (config, not a secret).
        let envs = ops.spawn_envs();
        assert_eq!(envs.len(), 1);
        assert_eq!(
            envs[0].get("SEARXNG_API_BASE").map(String::as_str),
            Some("http://127.0.0.1:8080")
        );
        // YAML gains the top-level search_tools block.
        let cfg = ops
            .file_bytes(&PathBuf::from("/data/gateway/config.yaml"))
            .expect("config written");
        let text = String::from_utf8(cfg).unwrap();
        assert!(text.contains("search_tools:"), "{text}");
        assert!(text.contains("search_provider: searxng"), "{text}");
        // Fingerprint records the SearXNG contribution.
        let state = ops
            .file_bytes(&crate::gateway_spawn_state_path(&ctx.data_dir))
            .expect("spawn state recorded");
        let state: serde_json::Value = serde_json::from_slice(&state).unwrap();
        assert_eq!(
            state["fingerprint"]["searxng_hash"].as_str().unwrap().len(),
            64,
            "{state}"
        );
    }

    #[test]
    fn start_gateway_planned_spawn_carries_searxng_real_value() {
        let ops = Arc::new(FakeSystemOps::new());
        let step = fast_step();
        let ctx = gateway_ctx_with_searxng(ops.clone(), "http://127.0.0.1:8080");
        let planned = step.planned_commands(&ctx);
        let plan_spawn = planned
            .iter()
            .find(|c| c.argv.iter().any(|a| a == "--config"))
            .expect("gateway spawn planned");
        // The real value, not a placeholder: api_base is config.
        assert_eq!(
            plan_spawn.env.get("SEARXNG_API_BASE").map(String::as_str),
            Some("http://127.0.0.1:8080")
        );
        // Disabled: no entry in the plan at all.
        let mut cfg = test_config_with_models();
        cfg.mcp.servers = vec![];
        let ctx = WizardContext::new(cfg, PathBuf::from("/data"), RunMode::Install, ops);
        let planned = step.planned_commands(&ctx);
        let plan_spawn = planned
            .iter()
            .find(|c| c.argv.iter().any(|a| a == "--config"))
            .expect("gateway spawn planned");
        assert!(!plan_spawn.env.contains_key("SEARXNG_API_BASE"));
    }

    #[tokio::test]
    async fn manual_start_block_carries_searxng_base() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = gateway_ctx_with_searxng(ops.clone(), "http://127.0.0.1:8080");
        let block = StartGateway::default().manual_start_block(&ctx).await;
        assert!(
            block.contains("$env:SEARXNG_API_BASE = \"http://127.0.0.1:8080\""),
            "{block}"
        );
    }

    #[tokio::test]
    async fn manual_start_block_carries_mcp_stdio_flag_and_credential_notes() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = gateway_ctx_with_mcp(ops.clone(), vec![mcp_stdio_gateway_server()]);
        let block = StartGateway::default().manual_start_block(&ctx).await;
        assert!(
            block.contains("$env:LITELLM_ENABLE_MCP_STDIO = \"true\""),
            "{block}"
        );
        assert!(block.contains("$env:BRAVE_API_KEY"), "{block}");
        assert!(
            block.contains("local-llm-service-manager/brave-search-mcp"),
            "{block}"
        );
    }

    /// (g) The installed prisma already matches the pin: no
    /// directory removal, no reinstall, no generation — the probe
    /// fast path runs as before.
    #[tokio::test]
    async fn start_gateway_keeps_matching_prisma_pin_untouched() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        with_postgres_installed(&ops, &ctx);
        let version_argv = prisma_version_argv(&step, &ctx);
        let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
        ops.with_command(&version_refs, FakeSystemOps::ok_output("0.11.0\n"));
        let probe_argv = prisma_probe_argv(&step, &ctx);
        let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
        ops.with_command(&probe_refs, FakeSystemOps::ok_output(""));
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(ops.removed_dirs().is_empty(), "{:?}", ops.removed_dirs());
        let executed = ops.executed_commands();
        assert!(
            !executed
                .iter()
                .any(|argv| argv.iter().any(|a| a == "--reinstall-package")),
            "no pin reinstall when the version matches: {executed:?}"
        );
        assert!(
            !executed
                .iter()
                .any(|argv| argv.iter().any(|a| a == "generate")),
            "{executed:?}"
        );
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("Prisma client verified")),
            "{out:?}"
        );
    }

    /// (g2) Verify-first fast path: pinned version + generated
    /// client present + connect probe passing means the start
    /// mutates NOTHING — no reinstall, no generation, no repair
    /// state — and records the spawn state for the drift check.
    /// This is the previously-running-state start: seconds, not
    /// minutes.
    #[tokio::test]
    async fn start_gateway_verifies_healthy_prisma_without_mutating() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        with_postgres_installed(&ops, &ctx);
        // The generated client's marker file is in place.
        ops.with_file(step.generated_client_path(&ctx), b"# generated".to_vec());
        let version_argv = prisma_version_argv(&step, &ctx);
        let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
        ops.with_command(&version_refs, FakeSystemOps::ok_output("0.11.0\n"));
        let probe_argv = prisma_probe_argv(&step, &ctx);
        let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
        ops.with_command(&probe_refs, FakeSystemOps::ok_output(""));
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.log_tail.iter().any(|l| l.contains("no changes made")),
            "{out:?}"
        );
        let executed = ops.executed_commands();
        assert!(
            !executed.iter().any(|argv| argv
                .iter()
                .any(|a| a == "--reinstall-package" || a == "generate")),
            "verification performed mutation: {executed:?}"
        );
        assert!(ops.removed_dirs().is_empty(), "{:?}", ops.removed_dirs());
        // No repair state was written (or left) on the fast path.
        assert!(ops
            .file_bytes(&StartGateway::prisma_repair_state_path(&ctx))
            .is_none());
        // The spawn state WAS recorded for the next drift check.
        let state = ops
            .file_bytes(&crate::gateway_spawn_state_path(&ctx.data_dir))
            .expect("spawn state recorded");
        let state: serde_json::Value = serde_json::from_slice(&state).unwrap();
        assert_eq!(state["fingerprint"]["port"], 4000);
        assert_eq!(state["fingerprint"]["postgres_port"], 54329);
        assert!(
            !state["fingerprint"]["master_key_hash"]
                .as_str()
                .unwrap()
                .is_empty(),
            "the spawned master key is fingerprinted: {state}"
        );
    }

    /// (g3) An interrupted repair (repair-state file present) is
    /// resumed and completed: the log says so, generation runs,
    /// and the state file is gone at the end.
    #[tokio::test]
    async fn start_gateway_resumes_interrupted_prisma_repair() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        with_postgres_installed(&ops, &ctx);
        // The leftover of an interrupted repair.
        ops.with_file(
            StartGateway::prisma_repair_state_path(&ctx),
            br#"{"pin":"0.11.0","phase":"pin"}"#.to_vec(),
        );
        let py = step.venv_python(&ctx).display().to_string();
        let schema = prisma_test_schema(&ctx);
        ops.with_file(schema.clone(), b"// schema".to_vec());
        let version_argv = prisma_version_argv(&step, &ctx);
        let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
        ops.with_command(&version_refs, FakeSystemOps::ok_output("0.11.0\n"));
        ops.with_command(
            &[py.as_str(), "-c", PRISMA_SCHEMA_SCRIPT],
            FakeSystemOps::ok_output(&format!("{}\n", schema.display())),
        );
        // The client was never generated (the interruption): the
        // probe fails, generation runs, the re-probe passes.
        let probe_argv = prisma_probe_argv(&step, &ctx);
        let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
        ops.with_command_sequence(
            &probe_refs,
            vec![prisma_not_generated_output(), FakeSystemOps::ok_output("")],
        );
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            out.log_tail.iter().any(|l| l.contains("did not complete")),
            "the resume is logged: {out:?}"
        );
        let executed = ops.executed_commands();
        assert_eq!(
            executed
                .iter()
                .filter(|argv| argv.iter().any(|a| a == "generate"))
                .count(),
            1,
            "{executed:?}"
        );
        assert!(
            ops.file_bytes(&StartGateway::prisma_repair_state_path(&ctx))
                .is_none(),
            "a completed repair clears its state"
        );
    }

    /// (h) The installed prisma is 0.15.0 — the state that passes a
    /// standalone connect but breaks LiteLLM's wrapper. The step
    /// must not trust the probe: it clears the stale generated
    /// client (the package directory), reinstalls the pinned
    /// package, and only then probes and generates.
    #[tokio::test]
    async fn start_gateway_reinstalls_pinned_prisma_and_clears_stale_client_on_mismatch() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        with_postgres_installed(&ops, &ctx);
        let py = step.venv_python(&ctx).display().to_string();
        let schema = prisma_test_schema(&ctx);
        ops.with_file(schema.clone(), b"// schema".to_vec());
        // The stale 0.15 package directory is present in the venv.
        ops.with_file(step.prisma_package_dir(&ctx), Vec::new());
        let version_argv = prisma_version_argv(&step, &ctx);
        let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
        ops.with_command(&version_refs, FakeSystemOps::ok_output("0.15.0\n"));
        ops.with_command(
            &[py.as_str(), "-c", PRISMA_SCHEMA_SCRIPT],
            FakeSystemOps::ok_output(&format!("{}\n", schema.display())),
        );
        // After the clear + reinstall the package is back to its
        // ungenerated wheel state: the first probe fails as
        // not-generated, generation runs, the re-probe passes.
        let probe_argv = prisma_probe_argv(&step, &ctx);
        let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
        ops.with_command_sequence(
            &probe_refs,
            vec![prisma_not_generated_output(), FakeSystemOps::ok_output("")],
        );
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        // The stale package directory was removed exactly once.
        assert_eq!(ops.removed_dirs(), vec![step.prisma_package_dir(&ctx)]);
        let executed = ops.executed_cmds();
        // The pin reinstall ran (exact pin, targeted reinstall)...
        let pin_at = executed
            .iter()
            .position(|c| c.argv.iter().any(|a| a == "--reinstall-package"))
            .expect("the pinned prisma reinstall ran");
        assert!(
            executed[pin_at]
                .argv
                .iter()
                .any(|a| a == &format!("prisma=={}", crate::PRISMA_PINNED_VERSION)),
            "{executed:?}"
        );
        // ... before the first connect probe.
        let probe_at = executed
            .iter()
            .position(|c| c.argv == probe_argv)
            .expect("the connect probe ran");
        assert!(pin_at < probe_at, "{executed:?}");
        // Generation ran once, after the reinstall.
        let generates = executed
            .iter()
            .filter(|c| c.argv.iter().any(|a| a == "generate"))
            .count();
        assert_eq!(generates, 1, "{executed:?}");
        // The mismatch was logged.
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("does not match the pinned")),
            "{out:?}"
        );
        assert!(ops.spawn_envs()[0].contains_key("DATABASE_URL"));
    }

    /// (i) Sustained health, database path: the gateway answers
    /// its first probe, then stops answering inside the
    /// confirmation window — the watchdog-kill shape. The step
    /// fails instead of reporting a gateway that is already gone.
    #[tokio::test]
    async fn start_gateway_db_path_fails_when_gateway_dies_inside_confirmation_window() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get_sequence(
            "http://127.0.0.1:4000/v1/models",
            vec![
                FakeSystemOps::http_ok(r#"{"data":[]}"#),
                crate::HttpResponse {
                    status: 503,
                    body: Vec::new(),
                    elapsed_ms: 1,
                },
            ],
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 50,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        with_postgres_installed(&ops, &ctx);
        let version_argv = prisma_version_argv(&step, &ctx);
        let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
        ops.with_command(&version_refs, FakeSystemOps::ok_output("0.11.0\n"));
        let probe_argv = prisma_probe_argv(&step, &ctx);
        let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
        ops.with_command(&probe_refs, FakeSystemOps::ok_output(""));
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(out.message.contains("did not stay up"), "{out:?}");
        assert!(out.manual_command.is_some(), "{out:?}");
        assert_eq!(ops.spawns().len(), 1);
    }

    /// (j) Sustained health, database path, gateway stays up: the
    /// step succeeds and the confirmation window actually probed
    /// (more than the single first-healthy GET).
    #[tokio::test]
    async fn start_gateway_db_path_sustained_health_passes() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 25,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        with_postgres_installed(&ops, &ctx);
        let version_argv = prisma_version_argv(&step, &ctx);
        let version_refs: Vec<&str> = version_argv.iter().map(String::as_str).collect();
        ops.with_command(&version_refs, FakeSystemOps::ok_output("0.11.0\n"));
        let probe_argv = prisma_probe_argv(&step, &ctx);
        let probe_refs: Vec<&str> = probe_argv.iter().map(String::as_str).collect();
        ops.with_command(&probe_refs, FakeSystemOps::ok_output(""));
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(
            ops.gets().len() > 1,
            "the confirmation window kept probing: {:?}",
            ops.gets()
        );
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("confirmation window")),
            "{out:?}"
        );
    }

    /// (k) Without a database there is no confirmation window,
    /// however long it is configured: the only GETs are the first
    /// healthy probe and the step's single final re-check — the
    /// configured 5 s window would have probed many more times.
    #[tokio::test]
    async fn start_gateway_without_database_skips_confirmation_window() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 5_000,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert_eq!(ops.gets().len(), 2, "{:?}", ops.gets());
    }

    /// (l) Spawn env on the database path: DATABASE_URL is set,
    /// the fresh-venv pip install carries the exact prisma pin,
    /// and the watchdog flag is present exactly when this build
    /// targets Windows (the defect it disables is Windows-only).
    /// Without a database, neither variable appears.
    #[tokio::test]
    async fn start_gateway_spawn_env_database_path() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let step = StartGateway {
            health_retries: 3,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        with_postgres_installed(&ops, &ctx);
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let env = &ops.spawn_envs()[0];
        assert!(env.contains_key("DATABASE_URL"), "{env:?}");
        assert_eq!(
            env.contains_key("PRISMA_HEALTH_WATCHDOG_ENABLED"),
            cfg!(target_os = "windows"),
            "{env:?}"
        );
        if let Some(v) = env.get("PRISMA_HEALTH_WATCHDOG_ENABLED") {
            assert_eq!(v, "false");
        }
        // The install that built this venv pinned prisma exactly.
        let executed = ops.executed_commands();
        assert!(
            executed.iter().any(|argv| argv
                .iter()
                .any(|a| a == &format!("prisma=={}", crate::PRISMA_PINNED_VERSION))),
            "{executed:?}"
        );
        // Database-free: neither variable.
        let ops2 = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops2);
        ops2.with_http_get(
            "http://127.0.0.1:4000/v1/models",
            FakeSystemOps::http_ok(r#"{"data":[]}"#),
        );
        let ctx2 = ctx_with(ops2.clone(), RunMode::Install);
        let out2 = step.run(&ctx2).await.unwrap();
        assert_eq!(out2.state, StepState::Done, "{out2:?}");
        let env2 = &ops2.spawn_envs()[0];
        assert!(!env2.contains_key("DATABASE_URL"), "{env2:?}");
        assert!(
            !env2.contains_key("PRISMA_HEALTH_WATCHDOG_ENABLED"),
            "{env2:?}"
        );
    }

    /// (e) Health-check failure: the failure MESSAGE is the step's
    /// own verdict; the spawned gateway's stderr tail rides in the
    /// step LOG, labeled as gateway output. (It previously sat in
    /// the message itself, presenting gateway log noise as the
    /// step's error.)
    #[tokio::test]
    async fn start_gateway_health_failure_includes_gateway_stderr_tail() {
        let ops = Arc::new(FakeSystemOps::new());
        with_bundled_uv(&ops);
        ops.with_dead_spawn();
        let step = StartGateway {
            health_retries: 2,
            health_interval_ms: 1,
            db_sustain_ms: 0,
        };
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        // The spawned gateway's captured stderr, planted where the
        // spawn redirects it.
        ops.with_file(
            step.gateway_stderr_log(&ctx),
            b"20:12:30 - LiteLLM Proxy:ERROR: utils.py:4432 - Failed to import Prisma client: The Client hasn't been generated yet\nRuntimeError: The Client hasn't been generated yet, you must run `prisma generate` before you can use the client.\n"
                .to_vec(),
        );
        let out = step.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(out.message.contains("did not become healthy"), "{out:?}");
        assert!(
            !out.message.contains("hasn't been generated"),
            "gateway output leaked into the step's verdict: {out:?}"
        );
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("gateway stderr") && l.contains("last lines")),
            "log carries the labeled gateway tail: {out:?}"
        );
        assert!(
            out.log_tail
                .iter()
                .any(|l| l.contains("The Client hasn't been generated yet")),
            "log carries the gateway's own diagnosis: {out:?}"
        );
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
            db_sustain_ms: 0,
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
            // Bootstrap download + the seven commands of the
            // uv-present plan (two probes, venv, pip, spawn, two
            // firewall commands).
            assert_eq!(planned.len(), 8);
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

    #[cfg(not(target_os = "windows"))]
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

    /// A context whose MCP registry is exactly `servers` (the
    /// default test config seeds the full registry, curated
    /// servers included; the Phase 4 step checks them all).
    fn ctx_with_mcp_servers(
        ops: Arc<FakeSystemOps>,
        servers: Vec<manager_config::McpServerConfig>,
    ) -> WizardContext {
        let mut config = test_config_with_models();
        config.mcp.servers = servers;
        WizardContext::new(config, PathBuf::from("/data"), RunMode::Install, ops)
    }

    fn mcp_test_server(
        id: &str,
        exposure: McpExposure,
        cred_ref: Option<&str>,
    ) -> manager_config::McpServerConfig {
        manager_config::McpServerConfig {
            id: id.to_string(),
            display_name: id.to_string(),
            transport: manager_config::McpTransport::Stdio,
            exposure,
            command: Some("npx".to_string()),
            args: vec!["-y".to_string(), format!("@example/{id}@1.0.0")],
            url: None,
            env_keys: vec!["EXAMPLE_API_KEY".to_string()],
            cred_ref: cred_ref.map(str::to_string),
            signup_url: None,
            credential_hint: None,
            allowed_tools: None,
            metered: false,
            rpm_limit: None,
        }
    }

    #[tokio::test]
    async fn mcp_servers_requires_credentials() {
        let ops = Arc::new(FakeSystemOps::new());
        let ctx = ctx_with(ops, RunMode::Install);
        let out = McpServers.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed);
        // Target name shown; values never shown (there are none to show).
        assert!(out.message.contains(GITHUB_CRED_TARGET), "{}", out.message);

        // Phase 4: the check is registry-wide, so the pass case
        // needs the curated brave_search credential too (the
        // default test config's registry seeds it Gateway-exposed).
        let ops2 = Arc::new(FakeSystemOps::new());
        ops2.with_credential(GITHUB_CRED_TARGET);
        ops2.with_credential(GDRIVE_CRED_TARGET);
        ops2.with_credential(manager_config::BRAVE_SEARCH_MCP_CRED_REF);
        let ctx2 = ctx_with(ops2.clone(), RunMode::Install);
        let out2 = McpServers.run(&ctx2).await.unwrap();
        assert_eq!(out2.state, StepState::Done, "{out2:?}");
        // Phase 1 (§10.1): the step writes nothing — the retired
        // mcp-servers.json store must not be created, and the success
        // log notes that it is inert.
        assert!(
            ops2.file_bytes(&PathBuf::from("/data/mcp/mcp-servers.json"))
                .is_none(),
            "mcp-servers.json must no longer be written"
        );
        let log_text = out2.log_tail.join("\n");
        assert!(
            log_text.contains("inert") || log_text.contains("no longer written"),
            "{log_text}"
        );
    }

    #[tokio::test]
    async fn mcp_servers_brave_search_missing_credential_fails() {
        // Phase 4 (§9): brave_search is Gateway-exposed with a
        // cred_ref, so its missing credential fails the step and
        // the failure names the server and its target.
        let brave = manager_config::BRAVE_SEARCH_MCP_CRED_REF;
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_credential(GITHUB_CRED_TARGET);
        ops.with_credential(GDRIVE_CRED_TARGET);
        let ctx = ctx_with(ops, RunMode::Install);
        let out = McpServers.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Failed, "{out:?}");
        assert!(out.message.contains("brave_search"), "{}", out.message);
        assert!(out.message.contains(brave), "{}", out.message);
        // The remediation names both ways out.
        let log_text = out.log_tail.join("\n");
        assert!(
            log_text.contains("store the credential") && log_text.contains("Disabled"),
            "{log_text}"
        );
        // Per-server log lines cover present and missing alike.
        assert!(
            log_text.contains("credential 'local-llm-service-manager/github-mcp' present"),
            "{log_text}"
        );
        assert!(
            log_text.contains(&format!("credential '{brave}' missing")),
            "{log_text}"
        );

        // With the Brave credential stored, the same registry passes.
        let ops2 = Arc::new(FakeSystemOps::new());
        ops2.with_credential(GITHUB_CRED_TARGET);
        ops2.with_credential(GDRIVE_CRED_TARGET);
        ops2.with_credential(brave);
        let ctx2 = ctx_with(ops2, RunMode::Install);
        let out2 = McpServers.run(&ctx2).await.unwrap();
        assert_eq!(out2.state, StepState::Done, "{out2:?}");
        assert!(out2.message.contains("brave_search"), "{}", out2.message);
    }

    #[tokio::test]
    async fn mcp_servers_disabled_server_credential_not_checked() {
        // A Disabled server is not checked at all: its missing
        // credential cannot fail the step (design §9).
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_credential("local-llm-service-manager/github-mcp");
        let ctx = ctx_with_mcp_servers(
            ops,
            vec![
                mcp_test_server(
                    "github",
                    McpExposure::Local,
                    Some("local-llm-service-manager/github-mcp"),
                ),
                mcp_test_server(
                    "web_fetch",
                    McpExposure::Disabled,
                    Some("local-llm-service-manager/web-fetch"),
                ),
            ],
        );
        let out = McpServers.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        assert!(out.message.contains("github"), "{}", out.message);
        assert!(!out.message.contains("web_fetch"), "{}", out.message);
    }

    #[tokio::test]
    async fn mcp_servers_notes_retired_file_inert_when_present() {
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_credential(GITHUB_CRED_TARGET);
        ops.with_credential(GDRIVE_CRED_TARGET);
        ops.with_credential(manager_config::BRAVE_SEARCH_MCP_CRED_REF);
        // A pre-Phase-1 file left on disk.
        ops.with_file(PathBuf::from("/data/mcp/mcp-servers.json"), b"{}".to_vec());
        let ctx = ctx_with(ops.clone(), RunMode::Install);
        let out = McpServers.run(&ctx).await.unwrap();
        assert_eq!(out.state, StepState::Done, "{out:?}");
        let log_text = out.log_tail.join("\n");
        assert!(log_text.contains("inert"), "{log_text}");
        // The file is left in place, untouched.
        assert_eq!(
            ops.file_bytes(&PathBuf::from("/data/mcp/mcp-servers.json"))
                .unwrap(),
            b"{}".to_vec()
        );
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

    /// argv of the `pg_isready` probe `postgres_ready` runs for a
    /// data dir, for scripting in the ensure-postgres tests below.
    fn pg_isready_argv(data_dir: &Path) -> Vec<String> {
        vec![
            postgres_bin_dir(data_dir)
                .join("pg_isready.exe")
                .display()
                .to_string(),
            "-h".to_string(),
            "127.0.0.1".to_string(),
            "-p".to_string(),
            "54329".to_string(),
        ]
    }

    fn argv_refs(argv: &[String]) -> Vec<&str> {
        argv.iter().map(String::as_str).collect()
    }

    fn pg_not_ready_output() -> CommandOutput {
        CommandOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "no response".to_string(),
        }
    }

    fn pg_ready_output() -> CommandOutput {
        CommandOutput {
            exit_code: 0,
            stdout: "127.0.0.1:54329 - accepting connections".to_string(),
            stderr: String::new(),
        }
    }

    /// Context for the free postgres functions in the tests
    /// below: they take the context for its config, data dir, ops
    /// and live sink.
    fn pg_ctx(ops: &Arc<FakeSystemOps>, cfg: AppConfig, data_dir: &Path) -> WizardContext {
        WizardContext::new(
            cfg,
            data_dir.to_path_buf(),
            RunMode::Install,
            ops.clone(),
        )
    }

    #[tokio::test]
    async fn postgres_start_adopts_live_postmaster_not_yet_ready() {
        // A postmaster from an earlier session is still starting
        // (pid file present, pid live and a postgres image, probe
        // not answering yet): it is awaited, never competed with.
        let ops = Arc::new(FakeSystemOps::new());
        let data_dir = PathBuf::from("/data");
        ops.with_file(postmaster_pid_path(&data_dir), b"4242\n/data\n".to_vec());
        ops.with_process_image(4242, "postgres.exe");
        let isready = pg_isready_argv(&data_dir);
        ops.with_command_sequence(
            &argv_refs(&isready),
            vec![pg_not_ready_output(), pg_ready_output()],
        );
        let cfg = test_config_with_models();
        let ctx = pg_ctx(&ops, cfg, &data_dir);
        let mut log = Vec::new();
        let url = ensure_postgres_running(&ctx, &mut log)
            .await
            .expect("adopts the running postmaster");
        assert!(url.contains("127.0.0.1:54329"), "{url}");
        assert!(
            log.iter().any(|l| l.contains("waiting for it")),
            "{log:?}"
        );
        assert!(
            !ops.executed_commands()
                .iter()
                .any(|a| a.iter().any(|x| x == "start")),
            "no competing pg_ctl start: {:?}",
            ops.executed_commands()
        );
    }

    #[tokio::test]
    async fn postgres_start_removes_stale_pid_file_with_dead_pid() {
        let ops = Arc::new(FakeSystemOps::new());
        let data_dir = PathBuf::from("/data");
        let pid_path = postmaster_pid_path(&data_dir);
        ops.with_file(pid_path.clone(), b"4242\n".to_vec());
        // 4242 is not scripted live: the recorded postmaster is gone.
        let isready = pg_isready_argv(&data_dir);
        ops.with_command_sequence(
            &argv_refs(&isready),
            vec![pg_not_ready_output(), pg_ready_output()],
        );
        let cfg = test_config_with_models();
        let ctx = pg_ctx(&ops, cfg, &data_dir);
        let mut log = Vec::new();
        ensure_postgres_running(&ctx, &mut log)
            .await
            .expect("starts after clearing the stale pid file");
        assert!(
            ops.file_bytes(&pid_path).is_none(),
            "stale pid file removed"
        );
        let start = postgres_start_command(&data_dir, 54329);
        assert!(
            ops.executed_commands().contains(&start.argv),
            "pg_ctl start ran: {:?}",
            ops.executed_commands()
        );
        // The start command carries pg_ctl's own wait bound.
        assert!(start.argv.iter().any(|a| a == "-t"), "{:?}", start.argv);
        assert!(start.argv.iter().any(|a| a == "120"), "{:?}", start.argv);
    }

    #[tokio::test]
    async fn postgres_start_removes_pid_file_recycled_by_another_program() {
        // The recorded pid is alive but belongs to an unrelated
        // program now (Windows pid reuse): the pid file is stale
        // even though the pid is not dead.
        let ops = Arc::new(FakeSystemOps::new());
        let data_dir = PathBuf::from("/data");
        let pid_path = postmaster_pid_path(&data_dir);
        ops.with_file(pid_path.clone(), b"4242\n".to_vec());
        ops.with_process_image(4242, "explorer.exe");
        let isready = pg_isready_argv(&data_dir);
        ops.with_command_sequence(
            &argv_refs(&isready),
            vec![pg_not_ready_output(), pg_ready_output()],
        );
        let cfg = test_config_with_models();
        let ctx = pg_ctx(&ops, cfg, &data_dir);
        let mut log = Vec::new();
        ensure_postgres_running(&ctx, &mut log)
            .await
            .expect("starts after clearing the recycled pid file");
        assert!(
            ops.file_bytes(&pid_path).is_none(),
            "recycled pid file removed"
        );
        let start = postgres_start_command(&data_dir, 54329);
        assert!(ops.executed_commands().contains(&start.argv));
    }

    #[tokio::test]
    async fn postgres_start_failure_carries_log_tail_when_pg_ctl_is_stopped() {
        // The 2026-10-18 field failure shape: the pg_ctl command
        // itself is stopped by a timeout (a WizardError, not an
        // exit code). The failure must carry the server log tail —
        // the old code attached it only to non-zero exits, leaving
        // exactly this path undiagnosable.
        let ops = Arc::new(FakeSystemOps::new());
        let data_dir = PathBuf::from("/data");
        let isready = pg_isready_argv(&data_dir);
        ops.with_command(&argv_refs(&isready), pg_not_ready_output());
        let start = postgres_start_command(&data_dir, 54329);
        ops.with_command_failure(
            &argv_refs(&start.argv),
            -1,
            "timed out after 180s and was stopped",
        );
        ops.with_file(
            postgres_log_path(&data_dir),
            b"2026-10-18 FATAL: the database system is starting up\nrecovery is in progress\n"
                .to_vec(),
        );
        let cfg = test_config_with_models();
        let ctx = pg_ctx(&ops, cfg, &data_dir);
        let mut log = Vec::new();
        let err = ensure_postgres_running(&ctx, &mut log)
            .await
            .expect_err("the start must fail");
        assert!(err.contains("timed out after 180s"), "{err}");
        assert!(err.contains("server log tail"), "{err}");
        assert!(err.contains("the database system is starting up"), "{err}");
    }

    #[tokio::test]
    async fn postgres_start_pg_ctl_failure_carries_log_tail() {
        // pg_ctl exits non-zero at once (the postmaster FATALed,
        // e.g. on the lock file): the settle poll sees no postmaster
        // and returns promptly; the failure carries pg_ctl's report
        // plus the server log tail.
        let ops = Arc::new(FakeSystemOps::new());
        let data_dir = PathBuf::from("/data");
        let isready = pg_isready_argv(&data_dir);
        ops.with_command(&argv_refs(&isready), pg_not_ready_output());
        let start = postgres_start_command(&data_dir, 54329);
        ops.with_command(
            &argv_refs(&start.argv),
            CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: "could not start server".to_string(),
            },
        );
        ops.with_file(
            postgres_log_path(&data_dir),
            b"FATAL: lock file \"postmaster.pid\" already exists\n".to_vec(),
        );
        let cfg = test_config_with_models();
        let ctx = pg_ctx(&ops, cfg, &data_dir);
        let mut log = Vec::new();
        let err = ensure_postgres_running(&ctx, &mut log)
            .await
            .expect_err("the start must fail");
        assert!(err.contains("pg_ctl exit 1"), "{err}");
        assert!(err.contains("server log tail"), "{err}");
        assert!(err.contains("lock file"), "{err}");
    }

    #[tokio::test]
    async fn postgres_wait_reports_gone_ready_and_timeout() {
        let data_dir = PathBuf::from("/data");
        let cfg = test_config_with_models();
        let isready = pg_isready_argv(&data_dir);
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_command(&argv_refs(&isready), pg_not_ready_output());
        let ctx = pg_ctx(&ops, cfg.clone(), &data_dir);
        let mut log = Vec::new();
        // Gone: a postmaster is required but no pid file exists.
        assert_eq!(
            wait_for_postgres_ready(&ctx, &data_dir, POSTGRES_SETTLE_WAIT, true, &mut log).await,
            PostgresWait::PostmasterGone
        );
        // TimedOut: no postmaster requirement, never ready, and a
        // zero window — returns without sleeping.
        assert_eq!(
            wait_for_postgres_ready(
                &ctx,
                &data_dir,
                std::time::Duration::ZERO,
                false,
                &mut log
            )
            .await,
            PostgresWait::TimedOut
        );
        // Ready: the probe answers on the second call.
        let ops2 = Arc::new(FakeSystemOps::new());
        ops2.with_command_sequence(
            &argv_refs(&isready),
            vec![pg_not_ready_output(), pg_ready_output()],
        );
        let ctx2 = pg_ctx(&ops2, cfg, &data_dir);
        assert_eq!(
            wait_for_postgres_ready(&ctx2, &data_dir, POSTGRES_SETTLE_WAIT, false, &mut log)
                .await,
            PostgresWait::Ready
        );
    }

    #[tokio::test]
    async fn postgres_log_streamer_emits_only_appended_lines() {
        // The readiness waits stream the postmaster's own log into
        // the step log: lines already present when the wait starts
        // are history (the failure tail covers them); lines
        // appended during the wait are this start's news and must
        // surface as they land.
        let ops = Arc::new(FakeSystemOps::new());
        let data_dir = PathBuf::from("/data");
        let log_path = postgres_log_path(&data_dir);
        ops.with_file(log_path.clone(), b"l1\nl2\nl3\n".to_vec());
        let ctx = pg_ctx(&ops, test_config_with_models(), &data_dir);
        let mut streamer = FileLineStreamer::new();
        streamer.prime(ops.as_ref(), &log_path).await;
        let mut log: Vec<String> = Vec::new();
        streamer
            .stream_new(&ctx, ops.as_ref(), &log_path, "postgres: ", &mut log)
            .await;
        assert!(log.is_empty(), "primed lines are not replayed: {log:?}");
        // The postmaster appends two lines (a FATAL among them).
        ops.with_file(
            log_path.clone(),
            b"l1\nl2\nl3\nl4\nFATAL: could not open lock file\n".to_vec(),
        );
        streamer
            .stream_new(&ctx, ops.as_ref(), &log_path, "postgres: ", &mut log)
            .await;
        assert_eq!(
            log,
            vec![
                "postgres: l4".to_string(),
                "postgres: FATAL: could not open lock file".to_string()
            ]
        );
        streamer
            .stream_new(&ctx, ops.as_ref(), &log_path, "postgres: ", &mut log)
            .await;
        assert_eq!(log.len(), 2, "no line is emitted twice: {log:?}");
    }

    #[tokio::test]
    async fn postgres_wait_ticks_report_elapsed_on_the_live_sink() {
        // With the tick set to every poll, a two-poll wait reports
        // itself twice on the live sink — the production cadence
        // (~10 s) is the same code with a larger tick.
        let data_dir = PathBuf::from("/data");
        let cfg = test_config_with_models();
        let isready = pg_isready_argv(&data_dir);
        let ops = Arc::new(FakeSystemOps::new());
        ops.with_command_sequence(
            &argv_refs(&isready),
            vec![
                pg_not_ready_output(),
                pg_not_ready_output(),
                pg_ready_output(),
            ],
        );
        let lines: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = lines.clone();
        let ctx = pg_ctx(&ops, cfg, &data_dir).with_log_sink(Arc::new(move |line: String| {
            captured.lock().unwrap().push(line)
        }));
        let mut log = Vec::new();
        let outcome = wait_for_postgres_ready_ticked(
            &ctx,
            &data_dir,
            POSTGRES_SETTLE_WAIT,
            false,
            &mut log,
            1,
        )
        .await;
        assert_eq!(outcome, PostgresWait::Ready);
        let lines = lines.lock().unwrap();
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.contains("waiting for PostgreSQL on 127.0.0.1:54329"))
                .count(),
            2,
            "{lines:?}"
        );
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
            db_sustain_ms: 0,
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
        assert!(install_cmds[2].argv[0].ends_with(StartGateway::uv_binary_name()));

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

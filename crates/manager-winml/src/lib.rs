//! Windows ML secondary backend (spec §6.5).
//!
//! `WinMLServer.exe` managed as an optional supervised process on :8090 and
//! registered in LiteLLM as a plain OpenAI-compatible backend. Constraints
//! enforced from research: single-GPU models only (no tensor-split surface
//! exists), coarse `--target gpu` with undocumented physical-GPU selection,
//! experimental status bannered in the UI.
//!
//! Eligible model class: tool-runner-sized single-GPU loads (e.g. a Qwen3-8B
//! instance) — never the planner or coder-30b. "Re-check capabilities"
//! re-probes and updates the banner.
//!
//! All HTTP goes through [`WinMlProbeTransport`] so tests are hermetic and the
//! crate builds and tests on Linux. Anything Windows-only lives behind
//! `cfg(windows)` shims (see [`winml_server_exe_path`]).

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use manager_config::WinMlConfig;
use serde::{Deserialize, Serialize};

/// Default WinMLServer port (spec §6.5: :8090 by convention).
pub const DEFAULT_PORT: u16 = 8090;

/// Ceiling for the WinML-eligible "small model" class: tool-runner-sized
/// single-GPU loads (e.g. Qwen3-8B Q4_K_M ≈ 5 GiB). Anything larger is a
/// planner/coder-class model and is refused for this secondary target.
pub const MAX_MODEL_SIZE_MIB: u64 = 8 * 1024;

/// How the physical GPU is selected (coarse: undocumented which adapter
/// responds; recorded at registration, warned on ambiguity).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuSelection {
    #[default]
    SystemDefault,
    TargetGpuCoarse,
}

/// Researched capability limits, recorded VERBATIM as data (spec §6.5).
/// Rendered for the UI by [`WinMlLimits::banner`].
pub const LIMIT_STATEMENTS: &[&str] = &[
    "No published Windows ML interface exposes llama.cpp multi-GPU tensor splitting.",
    "No reliable physical-GPU selection is exposed: GPU targeting is coarse (--target gpu) with undocumented physical-adapter choice.",
    "At most one model may be registered as a WinML secondary backend.",
    "Eligible model class is tool-runner-sized single-GPU loads only; never the planner or coder-30b.",
    "Windows ML support is experimental/preview.",
];

/// Researched capability limits. These are recorded verbatim and bannered in
/// the UI; if Microsoft later publishes tensor-split/device-index controls,
/// this struct is revisited — the primary path is not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinMlLimits {
    pub supports_tensor_split: bool,
    pub max_registered_models: u32,
    pub gpu_selection: GpuSelection,
    pub experimental: bool,
}

impl WinMlLimits {
    /// Limits as researched (Oct 2026).
    pub fn known_limits() -> Self {
        Self {
            supports_tensor_split: false,
            max_registered_models: 1,
            gpu_selection: GpuSelection::TargetGpuCoarse,
            experimental: true,
        }
    }

    /// The researched limits, verbatim, one statement per limit.
    pub fn statements() -> &'static [&'static str] {
        LIMIT_STATEMENTS
    }

    /// Render the limits as a UI banner: header plus every verbatim
    /// statement plus the effective values.
    pub fn banner(&self) -> String {
        let mut out = String::from("Windows ML secondary target — known limits:\n");
        for (i, s) in Self::statements().iter().enumerate() {
            out.push_str(&format!("{}. {}\n", i + 1, s));
        }
        out.push_str(&format!(
            "\nEffective: tensor_split={}, max_models={}, gpu_selection={:?}, experimental={}",
            self.supports_tensor_split,
            self.max_registered_models,
            self.gpu_selection,
            self.experimental,
        ));
        out
    }
}

/// A registered WinML backend instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinMlRegistration {
    pub model_id: String,
    pub port: u16,
    /// OpenAI-compatible endpoint, e.g. http://127.0.0.1:8090/v1.
    pub endpoint: String,
    pub registered_at: DateTime<Utc>,
    /// Which adapter actually responded at registration (ambiguity warned).
    pub responding_adapter: Option<String>,
    pub limits: WinMlLimits,
}

impl WinMlRegistration {
    /// Human-readable warning when the responding adapter could not be
    /// determined (spec §6.5: warn on ambiguity).
    pub fn ambiguity_warning(&self) -> Option<String> {
        if self.responding_adapter.is_none() {
            Some(format!(
                "Could not determine which physical GPU adapter responded for model '{}'; \
                 WinML GPU selection is coarse and undocumented. Verify load landed on the \
                 intended adapter via nvidia-smi.",
                self.model_id
            ))
        } else {
            None
        }
    }
}

/// Raw HTTP response from a probe transport.
#[derive(Debug, Clone)]
pub struct ProbeHttpResponse {
    pub status: u16,
    pub body: String,
    /// Best-effort hint about which adapter served the request (e.g. from a
    /// response header). Usually absent — absence yields an ambiguity warning.
    pub adapter_hint: Option<String>,
}

/// Abstraction over the HTTP GET used to probe WinMLServer. The real
/// implementation is [`ReqwestProbeTransport`]; tests use fakes.
#[async_trait]
pub trait WinMlProbeTransport: Send + Sync {
    async fn get(&self, url: &str) -> Result<ProbeHttpResponse, WinMlError>;
}

/// Real transport backed by reqwest. Cross-platform; on a machine without
/// WinMLServer the connection simply fails and the probe reports
/// [`WinMlError::Unavailable`].
pub struct ReqwestProbeTransport {
    client: reqwest::Client,
}

impl ReqwestProbeTransport {
    pub fn new(timeout: Duration) -> Result<Self, WinMlError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| WinMlError::TransportSetup(e.to_string()))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl WinMlProbeTransport for ReqwestProbeTransport {
    async fn get(&self, url: &str) -> Result<ProbeHttpResponse, WinMlError> {
        let resp = self.client.get(url).send().await.map_err(|e| {
            if e.is_timeout() {
                WinMlError::Timeout(e.to_string())
            } else if e.is_connect() {
                WinMlError::Unavailable(e.to_string())
            } else {
                WinMlError::ProbeFailed(e.to_string())
            }
        })?;
        let status = resp.status().as_u16();
        // Best-effort adapter hint; WinMLServer does not document one.
        let adapter_hint = resp
            .headers()
            .get("x-winml-adapter")
            .or_else(|| resp.headers().get("x-gpu-adapter"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = resp
            .text()
            .await
            .map_err(|e| WinMlError::ProbeFailed(e.to_string()))?;
        Ok(ProbeHttpResponse {
            status,
            body,
            adapter_hint,
        })
    }
}

/// Result of probing a WinMLServer instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinMlProbeReport {
    /// Base URL that was probed, e.g. http://127.0.0.1:8090.
    pub base_url: String,
    /// Model ids advertised by GET /v1/models (OpenAI list format).
    pub models: Vec<String>,
    /// Adapter hint from the probe, if any.
    pub adapter_hint: Option<String>,
    /// Limits recorded verbatim at probe time.
    pub limits: WinMlLimits,
}

/// OpenAI-compatible endpoint for a port: http://127.0.0.1:{port}/v1.
pub fn default_endpoint(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

/// Base URL (no /v1 suffix) for a port.
pub fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Probe WinMLServer availability at `base_url` and record capability limits
/// verbatim. Issues HTTP GET against the OpenAI-compatible `/v1/models`
/// endpoint via the provided transport.
pub async fn probe(
    transport: &dyn WinMlProbeTransport,
    base_url: &str,
) -> Result<WinMlProbeReport, WinMlError> {
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    let resp = transport.get(&url).await?;
    if resp.status != 200 {
        return Err(WinMlError::UnexpectedStatus(resp.status));
    }
    let models = parse_models_list(&resp.body)?;
    Ok(WinMlProbeReport {
        base_url: base_url.to_string(),
        models,
        adapter_hint: resp.adapter_hint,
        limits: WinMlLimits::known_limits(),
    })
}

/// Parse an OpenAI `/v1/models` list body into model ids.
/// Accepts `{"object":"list","data":[{"id":"..."}, ...]}` and the bare
/// `{"data":[...]}` shape; `id` may also appear as `name` in some servers.
fn parse_models_list(body: &str) -> Result<Vec<String>, WinMlError> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| WinMlError::BadResponse(e.to_string()))?;
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| WinMlError::BadResponse("missing 'data' array".to_string()))?;
    let mut ids = Vec::with_capacity(data.len());
    for entry in data {
        let id = entry
            .get("id")
            .or_else(|| entry.get("name"))
            .and_then(|s| s.as_str())
            .ok_or_else(|| WinMlError::BadResponse("model entry without id".to_string()))?;
        ids.push(id.to_string());
    }
    Ok(ids)
}

/// Model ids that are never eligible for the WinML secondary target,
/// regardless of size heuristics.
const INELIGIBLE_MODEL_IDS: &[&str] = &["planner", "coder-30b", "coder30b"];

/// Check that `model_id` names a tool-runner-class model. Refuses the planner
/// and coder-30b explicitly, plus anything whose id advertises more than 14B
/// parameters (e.g. "qwen3-27b", "model-30b"). Heuristic and documented:
/// callers with catalog metadata should prefer the size check.
pub fn model_class_eligible(model_id: &str) -> Result<(), WinMlError> {
    let lower = model_id.to_lowercase();
    if INELIGIBLE_MODEL_IDS.iter().any(|b| lower.contains(b)) {
        return Err(WinMlError::IneligibleModel(model_id.to_string()));
    }
    // Parse "<n>b" parameter counts, e.g. "qwen3-27b" -> 27.
    let mut chars = lower.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut num = String::from(c);
            loop {
                match chars.peek() {
                    Some(d) if d.is_ascii_digit() => {
                        num.push(*d);
                        chars.next();
                    }
                    _ => break,
                }
            }
            if matches!(chars.peek(), Some('b')) {
                if let Ok(n) = num.parse::<u64>() {
                    if n > 14 {
                        return Err(WinMlError::IneligibleModel(format!(
                            "{model_id} advertises {n}B parameters (> 14B tool-runner class)"
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Options for [`WinMlRegistry::register`].
#[derive(Debug, Clone, Default)]
pub struct RegisterOptions {
    /// Model size in MiB, when known from the catalog. Enforced against
    /// [`MAX_MODEL_SIZE_MIB`]; `None` skips the size check (documented gap).
    pub model_size_mib: Option<u64>,
    pub gpu_selection: GpuSelection,
}

/// Stateful registry enforcing the single-model rule (spec §6.5:
/// `max_registered_models = 1`).
pub struct WinMlRegistry {
    registration: Option<WinMlRegistration>,
    transport: Box<dyn WinMlProbeTransport>,
}

impl WinMlRegistry {
    pub fn new(transport: impl WinMlProbeTransport + 'static) -> Self {
        Self {
            registration: None,
            transport: Box::new(transport),
        }
    }

    /// Register one small (tool-runner-class) model as a secondary LiteLLM
    /// backend. Refuses planner/coder-30b-class models, oversize models, and
    /// a second registration while one is active.
    pub async fn register(
        &mut self,
        config: &WinMlConfig,
        model_id: &str,
        opts: &RegisterOptions,
    ) -> Result<&WinMlRegistration, WinMlError> {
        if let Some(existing) = &self.registration {
            return Err(WinMlError::AlreadyRegistered(existing.model_id.clone()));
        }
        if !config.enabled {
            return Err(WinMlError::RegistrationFailed(
                "WinML backend is disabled in config".to_string(),
            ));
        }
        model_class_eligible(model_id)?;
        if let Some(size) = opts.model_size_mib {
            if size > MAX_MODEL_SIZE_MIB {
                return Err(WinMlError::ModelTooLarge {
                    model_id: model_id.to_string(),
                    size_mib: size,
                    max_mib: MAX_MODEL_SIZE_MIB,
                });
            }
        }
        let port = if config.port == 0 {
            DEFAULT_PORT
        } else {
            config.port
        };
        let base = base_url(port);
        let report = probe(self.transport.as_ref(), &base).await?;
        // If the server advertises a non-empty model list, the model we want
        // must be among them; otherwise the server is serving something else.
        if !report.models.is_empty() && !report.models.iter().any(|m| m == model_id) {
            return Err(WinMlError::RegistrationFailed(format!(
                "WinMLServer at {base} does not serve '{model_id}' (serves: {})",
                report.models.join(", ")
            )));
        }
        let mut registration = WinMlRegistration {
            model_id: model_id.to_string(),
            port,
            endpoint: default_endpoint(port),
            registered_at: Utc::now(),
            responding_adapter: report.adapter_hint,
            limits: WinMlLimits::known_limits(),
        };
        // Record the requested GPU selection alongside the researched limits.
        registration.limits.gpu_selection = opts.gpu_selection;
        Ok(self.registration.insert(registration))
    }

    /// Unregister the WinML backend, returning the removed registration.
    pub fn unregister(&mut self) -> Option<WinMlRegistration> {
        self.registration.take()
    }

    /// Current registration, if any.
    pub fn status(&self) -> Option<&WinMlRegistration> {
        self.registration.as_ref()
    }

    /// Re-probe capabilities ("Re-check capabilities" in the UI) and refresh
    /// the banner limits on the active registration.
    pub async fn recheck_capabilities(&mut self) -> Result<WinMlProbeReport, WinMlError> {
        let port = self
            .registration
            .as_ref()
            .map(|r| r.port)
            .unwrap_or(DEFAULT_PORT);
        let report = probe(self.transport.as_ref(), &base_url(port)).await?;
        if let Some(reg) = self.registration.as_mut() {
            reg.limits = report.limits.clone();
            if reg.responding_adapter.is_none() {
                reg.responding_adapter = report.adapter_hint.clone();
            }
        }
        Ok(report)
    }

    /// Health probe against the /v1 endpoint of the registered (or default)
    /// port. `Ok(true)` = 200 on /v1/models; `Ok(false)` = reachable but
    /// unexpected status; `Err` = transport failure.
    pub async fn health(&self) -> Result<bool, WinMlError> {
        let port = self
            .registration
            .as_ref()
            .map(|r| r.port)
            .unwrap_or(DEFAULT_PORT);
        health(self.transport.as_ref(), port).await
    }
}

/// Health probe against the /v1 endpoint on `port`.
/// `Ok(true)` = HTTP 200 on /v1/models; `Ok(false)` = reachable but non-200;
/// `Err` = transport failure.
pub async fn health(transport: &dyn WinMlProbeTransport, port: u16) -> Result<bool, WinMlError> {
    let url = format!("{}/v1/models", base_url(port));
    let resp = transport.get(&url).await?;
    Ok(resp.status == 200)
}

/// Default install location of WinMLServer.exe on Windows.
///
/// Marked `cfg(windows)`: off Windows this is a placeholder and must never be
/// treated as a real path.
#[cfg(windows)]
pub fn winml_server_exe_path() -> PathBuf {
    // Best-known location; the wizard verifies existence before use.
    PathBuf::from(r"C:\Program Files\WindowsML\WinMLServer.exe")
}

/// Non-Windows placeholder: there is no WinMLServer off Windows.
#[cfg(not(windows))]
pub fn winml_server_exe_path() -> PathBuf {
    PathBuf::from("WinMLServer.exe")
}

/// WinML errors.
#[derive(Debug, thiserror::Error)]
pub enum WinMlError {
    #[error("Windows ML unavailable: {0}")]
    Unavailable(String),
    #[error("registration failed: {0}")]
    RegistrationFailed(String),
    #[error("model class not eligible for WinML secondary target: {0}")]
    IneligibleModel(String),
    #[error("a WinML model is already registered ('{0}'); unregister it first")]
    AlreadyRegistered(String),
    #[error(
        "model '{model_id}' is {size_mib} MiB, over the WinML small-model cap of {max_mib} MiB"
    )]
    ModelTooLarge {
        model_id: String,
        size_mib: u64,
        max_mib: u64,
    },
    #[error("probe timed out: {0}")]
    Timeout(String),
    #[error("probe failed: {0}")]
    ProbeFailed(String),
    #[error("transport setup failed: {0}")]
    TransportSetup(String),
    #[error("unexpected HTTP status from WinMLServer: {0}")]
    UnexpectedStatus(u16),
    #[error("unparseable /v1/models response: {0}")]
    BadResponse(String),
    #[error("health check failed: {0}")]
    HealthCheckFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hermetic fake transport for tests.
    struct FakeTransport {
        status: u16,
        body: String,
        adapter_hint: Option<String>,
        fail: Option<WinMlError>,
    }

    impl FakeTransport {
        fn healthy() -> Self {
            Self {
                status: 200,
                body: r#"{"object":"list","data":[{"id":"qwen3-8b","object":"model"}]}"#
                    .to_string(),
                adapter_hint: Some("NVIDIA RTX A4000".to_string()),
                fail: None,
            }
        }

        fn failing(err: WinMlError) -> Self {
            Self {
                status: 0,
                body: String::new(),
                adapter_hint: None,
                fail: Some(err),
            }
        }
    }

    #[async_trait]
    impl WinMlProbeTransport for FakeTransport {
        async fn get(&self, _url: &str) -> Result<ProbeHttpResponse, WinMlError> {
            if let Some(e) = &self.fail {
                return Err(match e {
                    WinMlError::Timeout(s) => WinMlError::Timeout(s.clone()),
                    WinMlError::Unavailable(s) => WinMlError::Unavailable(s.clone()),
                    WinMlError::ProbeFailed(s) => WinMlError::ProbeFailed(s.clone()),
                    _ => WinMlError::ProbeFailed("fake failure".to_string()),
                });
            }
            Ok(ProbeHttpResponse {
                status: self.status,
                body: self.body.clone(),
                adapter_hint: self.adapter_hint.clone(),
            })
        }
    }

    fn enabled_config() -> WinMlConfig {
        WinMlConfig {
            enabled: true,
            port: 8090,
            model_id: None,
        }
    }

    #[test]
    fn winml_limits_have_no_tensor_split() {
        let limits = WinMlLimits::known_limits();
        assert!(!limits.supports_tensor_split);
        assert_eq!(limits.max_registered_models, 1);
        assert!(limits.experimental);
        assert_eq!(limits.gpu_selection, GpuSelection::TargetGpuCoarse);
    }

    #[test]
    fn limit_statements_cover_every_limit() {
        let stmts = WinMlLimits::statements();
        assert_eq!(stmts.len(), 5);
        let joined = stmts.join("\n").to_lowercase();
        for keyword in [
            "tensor splitting",
            "physical-gpu",
            "one model",
            "tool-runner",
            "experimental",
        ] {
            assert!(
                joined.contains(keyword),
                "missing verbatim limit statement for '{keyword}'"
            );
        }
    }

    #[test]
    fn banner_contains_every_recorded_limit() {
        let banner = WinMlLimits::known_limits().banner();
        for stmt in WinMlLimits::statements() {
            assert!(
                banner.contains(stmt),
                "banner is missing verbatim statement: {stmt}"
            );
        }
        // Effective values are rendered too.
        assert!(banner.contains("tensor_split=false"));
        assert!(banner.contains("max_models=1"));
        assert!(banner.contains("experimental=true"));
    }

    #[test]
    fn default_endpoint_format() {
        assert_eq!(default_endpoint(8090), "http://127.0.0.1:8090/v1");
        assert_eq!(default_endpoint(9000), "http://127.0.0.1:9000/v1");
        assert_eq!(base_url(8090), "http://127.0.0.1:8090");
    }

    #[test]
    fn model_class_eligibility() {
        // Eligible: tool-runner-sized ids.
        for ok in [
            "qwen3-8b",
            "Qwen2.5-Coder-7B",
            "tool-runner",
            "llama-8b-chat",
        ] {
            assert!(
                model_class_eligible(ok).is_ok(),
                "expected '{ok}' to be eligible"
            );
        }
        // Ineligible: explicit blocklist.
        for bad in [
            "planner",
            "Planner-Instance",
            "qwen3-coder-30b",
            "coder30b-x",
        ] {
            assert!(
                matches!(
                    model_class_eligible(bad),
                    Err(WinMlError::IneligibleModel(_))
                ),
                "expected '{bad}' to be rejected"
            );
        }
        // Ineligible: >14B parameters advertised in the id.
        for bad in ["qwen3-27b", "model-30b", "mixtral-22b"] {
            assert!(
                matches!(
                    model_class_eligible(bad),
                    Err(WinMlError::IneligibleModel(_))
                ),
                "expected '{bad}' to be rejected"
            );
        }
    }

    #[tokio::test]
    async fn probe_parses_openai_models_list() {
        let report = probe(&FakeTransport::healthy(), "http://127.0.0.1:8090")
            .await
            .unwrap();
        assert_eq!(report.models, vec!["qwen3-8b".to_string()]);
        assert_eq!(report.adapter_hint.as_deref(), Some("NVIDIA RTX A4000"));
        assert!(!report.limits.supports_tensor_split);
    }

    #[tokio::test]
    async fn probe_rejects_non_200() {
        let t = FakeTransport {
            status: 503,
            ..FakeTransport::healthy()
        };
        let err = probe(&t, "http://127.0.0.1:8090").await.unwrap_err();
        assert!(matches!(err, WinMlError::UnexpectedStatus(503)));
    }

    #[tokio::test]
    async fn probe_rejects_malformed_body() {
        let t = FakeTransport {
            body: "not json".to_string(),
            ..FakeTransport::healthy()
        };
        let err = probe(&t, "http://127.0.0.1:8090").await.unwrap_err();
        assert!(matches!(err, WinMlError::BadResponse(_)));
    }

    #[tokio::test]
    async fn probe_propagates_timeout() {
        let t = FakeTransport::failing(WinMlError::Timeout("deadline".to_string()));
        let err = probe(&t, "http://127.0.0.1:8090").await.unwrap_err();
        assert!(matches!(err, WinMlError::Timeout(_)));
    }

    #[tokio::test]
    async fn probe_propagates_unavailable() {
        let t = FakeTransport::failing(WinMlError::Unavailable("refused".to_string()));
        let err = probe(&t, "http://127.0.0.1:8090").await.unwrap_err();
        assert!(matches!(err, WinMlError::Unavailable(_)));
    }

    #[tokio::test]
    async fn register_happy_path() {
        let mut reg = WinMlRegistry::new(FakeTransport::healthy());
        let opts = RegisterOptions {
            model_size_mib: Some(5 * 1024),
            gpu_selection: GpuSelection::SystemDefault,
        };
        let r = reg
            .register(&enabled_config(), "qwen3-8b", &opts)
            .await
            .unwrap();
        assert_eq!(r.model_id, "qwen3-8b");
        assert_eq!(r.port, 8090);
        assert_eq!(r.endpoint, "http://127.0.0.1:8090/v1");
        assert_eq!(r.responding_adapter.as_deref(), Some("NVIDIA RTX A4000"));
        assert!(r.ambiguity_warning().is_none());
        assert!(reg.status().is_some());
    }

    #[tokio::test]
    async fn register_uses_port_default_when_zero() {
        let mut reg = WinMlRegistry::new(FakeTransport::healthy());
        let mut cfg = enabled_config();
        cfg.port = 0;
        let opts = RegisterOptions::default();
        let r = reg.register(&cfg, "qwen3-8b", &opts).await.unwrap();
        assert_eq!(r.port, DEFAULT_PORT);
        assert_eq!(r.endpoint, default_endpoint(DEFAULT_PORT));
    }

    #[tokio::test]
    async fn register_rejects_second_model() {
        let mut reg = WinMlRegistry::new(FakeTransport::healthy());
        let opts = RegisterOptions::default();
        reg.register(&enabled_config(), "qwen3-8b", &opts)
            .await
            .unwrap();
        let err = reg
            .register(&enabled_config(), "qwen2.5-coder-7b", &opts)
            .await
            .unwrap_err();
        assert!(matches!(err, WinMlError::AlreadyRegistered(_)));
        // First registration is untouched.
        assert_eq!(reg.status().unwrap().model_id, "qwen3-8b");
    }

    #[tokio::test]
    async fn register_rejects_ineligible_model_class() {
        let mut reg = WinMlRegistry::new(FakeTransport::healthy());
        let opts = RegisterOptions::default();
        for bad in ["planner", "qwen3-coder-30b"] {
            let err = reg
                .register(&enabled_config(), bad, &opts)
                .await
                .unwrap_err();
            assert!(
                matches!(err, WinMlError::IneligibleModel(_)),
                "expected IneligibleModel for '{bad}'"
            );
        }
        assert!(reg.status().is_none());
    }

    #[tokio::test]
    async fn register_rejects_oversize_model() {
        let mut reg = WinMlRegistry::new(FakeTransport::healthy());
        let opts = RegisterOptions {
            model_size_mib: Some(16 * 1024),
            ..Default::default()
        };
        let err = reg
            .register(&enabled_config(), "qwen3-8b", &opts)
            .await
            .unwrap_err();
        match err {
            WinMlError::ModelTooLarge {
                size_mib, max_mib, ..
            } => {
                assert_eq!(size_mib, 16 * 1024);
                assert_eq!(max_mib, MAX_MODEL_SIZE_MIB);
            }
            other => panic!("expected ModelTooLarge, got {other:?}"),
        }
        assert!(reg.status().is_none());
    }

    #[tokio::test]
    async fn register_rejects_when_disabled() {
        let mut reg = WinMlRegistry::new(FakeTransport::healthy());
        let mut cfg = enabled_config();
        cfg.enabled = false;
        let err = reg
            .register(&cfg, "qwen3-8b", &RegisterOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, WinMlError::RegistrationFailed(_)));
    }

    #[tokio::test]
    async fn register_rejects_model_mismatch() {
        // Server serves a different model than the one requested.
        let mut reg = WinMlRegistry::new(FakeTransport::healthy());
        let err = reg
            .register(
                &enabled_config(),
                "qwen2.5-coder-7b",
                &RegisterOptions::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WinMlError::RegistrationFailed(_)));
    }

    #[tokio::test]
    async fn register_fails_when_server_unreachable() {
        let mut reg = WinMlRegistry::new(FakeTransport::failing(WinMlError::Unavailable(
            "connection refused".to_string(),
        )));
        let err = reg
            .register(&enabled_config(), "qwen3-8b", &RegisterOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, WinMlError::Unavailable(_)));
        assert!(reg.status().is_none());
    }

    #[tokio::test]
    async fn register_warns_on_ambiguous_adapter() {
        let t = FakeTransport {
            adapter_hint: None,
            ..FakeTransport::healthy()
        };
        let mut reg = WinMlRegistry::new(t);
        let r = reg
            .register(&enabled_config(), "qwen3-8b", &RegisterOptions::default())
            .await
            .unwrap();
        let warning = r.ambiguity_warning().expect("expected ambiguity warning");
        assert!(warning.contains("qwen3-8b"));
        assert!(warning.to_lowercase().contains("physical gpu"));
    }

    #[tokio::test]
    async fn unregister_clears_registration() {
        let mut reg = WinMlRegistry::new(FakeTransport::healthy());
        reg.register(&enabled_config(), "qwen3-8b", &RegisterOptions::default())
            .await
            .unwrap();
        let removed = reg.unregister().expect("expected a registration");
        assert_eq!(removed.model_id, "qwen3-8b");
        assert!(reg.status().is_none());
        assert!(reg.unregister().is_none());
    }

    #[tokio::test]
    async fn recheck_capabilities_refreshes_limits() {
        let mut reg = WinMlRegistry::new(FakeTransport::healthy());
        reg.register(&enabled_config(), "qwen3-8b", &RegisterOptions::default())
            .await
            .unwrap();
        let report = reg.recheck_capabilities().await.unwrap();
        assert_eq!(report.models, vec!["qwen3-8b".to_string()]);
        assert!(!reg.status().unwrap().limits.supports_tensor_split);
    }

    #[tokio::test]
    async fn health_true_on_200_false_on_500() {
        assert!(health(&FakeTransport::healthy(), 8090).await.unwrap());
        let t = FakeTransport {
            status: 500,
            ..FakeTransport::healthy()
        };
        assert!(!health(&t, 8090).await.unwrap());
    }

    #[tokio::test]
    async fn health_errors_on_transport_failure() {
        let t = FakeTransport::failing(WinMlError::Timeout("t".to_string()));
        let err = health(&t, 8090).await.unwrap_err();
        assert!(matches!(err, WinMlError::Timeout(_)));
    }

    #[tokio::test]
    async fn registry_health_uses_registered_port() {
        let mut reg = WinMlRegistry::new(FakeTransport::healthy());
        assert!(reg.health().await.unwrap());
        reg.register(&enabled_config(), "qwen3-8b", &RegisterOptions::default())
            .await
            .unwrap();
        assert!(reg.health().await.unwrap());
    }

    #[test]
    fn registration_serializes() {
        let r = WinMlRegistration {
            model_id: "qwen3-8b".to_string(),
            port: 8090,
            endpoint: default_endpoint(8090),
            registered_at: Utc::now(),
            responding_adapter: None,
            limits: WinMlLimits::known_limits(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: WinMlRegistration = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model_id, "qwen3-8b");
        assert!(!back.limits.supports_tensor_split);
    }
}

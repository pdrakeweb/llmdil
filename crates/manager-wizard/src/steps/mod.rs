//! The ten concrete wizard steps (spec §4.1).
//!
//! Steps branch on [`RunMode`]: in `Install` they perform the install; in
//! `Audit` they re-verify without reinstalling. All system effects go through
//! [`SystemOps`](crate::SystemOps), so every step is hermetic under
//! [`FakeSystemOps`](crate::fake::FakeSystemOps).

use crate::{
    apply_config_edits, checksums_match, is_pinned_hash, CommandOutput, ConfigEdit, ConfigEditMode,
    PlannedCommand, RunMode, StepLogSink, StepOutcome, SystemOps, WizardContext, WizardError,
    WizardStep,
};
use async_trait::async_trait;
use manager_config::{
    AppConfig, BackendConfig, McpExposure, ModelConfig, SplitMode, SupervisionMode,
};
use std::path::{Path, PathBuf};

mod client_config;
mod common;
mod detect_hardware;
mod download_models;
mod gateway;
mod llamacpp;
mod mcp_servers;
mod mxc;
mod postgres;
mod smoke_test;
mod tensor_split;
mod vscode_extensions;
mod winml_register;

pub use self::client_config::*;
use self::common::*;
pub use self::detect_hardware::*;
pub use self::download_models::*;
pub use self::gateway::*;
pub use self::llamacpp::*;
pub use self::mcp_servers::*;
pub use self::mxc::*;
pub use self::postgres::*;
pub use self::smoke_test::*;
pub use self::tensor_split::*;
pub use self::vscode_extensions::*;
pub use self::winml_register::*;

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

/// Steps setup can be finished without: editor integration, MCP
/// servers, Windows ML and the MXC sandbox.
pub const OPTIONAL_STEPS: &[&str] = &[
    STEP_VSCODE_EXTENSIONS,
    STEP_MCP_SERVERS,
    STEP_WINML_REGISTER,
    STEP_INSTALL_MXC,
    STEP_MXC_POLICIES,
];

/// Resolve a possibly-relative path against the data dir.
fn resolve_dest(data_dir: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        data_dir.join(p)
    }
}

/// Where a model file is across the storage tiers
/// (docs/tiered-model-storage.md), probed through `SystemOps` so the
/// steps stay hermetic under test.
pub(crate) struct ModelLocation {
    /// The first tier copy found (hot first), if any.
    pub(crate) found: Option<PathBuf>,
    /// The hot-tier path: the download destination for a missing file.
    pub(crate) hot: PathBuf,
    /// A configured cold tier's root is unreachable, so a missing file
    /// may be on it: never re-download in that case.
    pub(crate) cold_unavailable: bool,
}

impl ModelLocation {
    /// The path to verify or download to.
    pub(crate) fn dest(&self) -> &Path {
        self.found.as_deref().unwrap_or(&self.hot)
    }
}

pub(crate) async fn locate_model(ctx: &WizardContext, logical: &Path) -> ModelLocation {
    let hot = resolve_dest(&ctx.data_dir, logical);
    if ctx.ops.file_exists(&hot).await {
        return ModelLocation {
            found: Some(hot.clone()),
            hot,
            cold_unavailable: false,
        };
    }
    let mut cold_unavailable = false;
    if let Some(rel) = manager_config::tier_relative(logical) {
        for cold in &ctx.config.model_storage.cold_tiers {
            if !ctx.ops.file_exists(&cold.path).await {
                cold_unavailable = true;
                continue;
            }
            let candidate = cold.path.join(&rel);
            if ctx.ops.file_exists(&candidate).await {
                return ModelLocation {
                    found: Some(candidate),
                    hot,
                    cold_unavailable: false,
                };
            }
        }
    }
    ModelLocation {
        found: None,
        hot,
        cold_unavailable,
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

#[cfg(test)]
mod tests;

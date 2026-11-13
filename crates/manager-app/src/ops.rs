//! The operation implementations behind the registry, grouped by family.
//! Every item is re-exported at the crate root (`lib.rs`), so registry
//! handlers and downstream crates keep their `crate::op_*` paths.

pub(crate) mod backend_config;
pub(crate) mod backend_info;
pub(crate) mod backend_spec;
pub(crate) mod backend_tuning;
pub(crate) mod backends;
pub(crate) mod config;
pub(crate) mod gateway;
pub(crate) mod gateway_activation;
pub(crate) mod logs;
pub(crate) mod mcp;
pub(crate) mod models;
pub(crate) mod offload;
pub(crate) mod placement;
pub(crate) mod platform;
pub(crate) mod preflight;
pub(crate) mod services;
pub(crate) mod telemetry;
pub(crate) mod wizard;

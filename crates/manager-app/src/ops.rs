//! The operation implementations behind the registry, grouped by family.
//! Every item is re-exported at the crate root (`lib.rs`), so registry
//! handlers and downstream crates keep their `crate::op_*` paths.

pub(crate) mod config;
pub(crate) mod logs;
pub(crate) mod mcp;
pub(crate) mod platform;
pub(crate) mod services;
pub(crate) mod wizard;

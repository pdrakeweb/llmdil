//! LLMdil Admin API client library (CLI/TUI shared core).
//!
//! See the crate README for usage. Security rules enforced here:
//! - Connections require a matching certificate fingerprint (see
//!   [`pinning`]); a mismatch aborts the connection.
//! - Tokens live in memory only, never on disk or in config files.
//! - No secret (token, password, enrollment code, TOTP code) is ever
//!   accepted as a command-line argument; secrets arrive via
//!   environment or stdin only.

pub mod client;
pub mod pinning;

pub use client::{ApiClient, ApiError, LoginOutcome, Operation, TokenPair};
pub use pinning::parse_fingerprint;

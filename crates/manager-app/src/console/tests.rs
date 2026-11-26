//! Unit tests for the console's machine-facing pieces; the end-to-end
//! runs (fake executable, backend stop/restore, artifacts) are in
//! `tests/console_run.rs`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::json;

use super::plan::{allowlisted, display_command, is_loopback_url};
use super::runner::synthetic_prompt;
use super::Caller;
use crate::registry::CapClass;

#[test]
fn desktop_caller_is_local_admin_and_api_roles_are_enforced() {
    let ui = Caller::from_arg(None);
    assert!(!ui.is_api() && ui.loopback);
    assert!(ui.require_class(CapClass::D, "x").is_ok());
    let viewer = Caller::from_arg(Some(
        json!({ "kind": "api", "actor": "v", "role": "viewer" }),
    ));
    assert!(viewer.require_class(CapClass::R, "read").is_ok());
    assert!(viewer.require_class(CapClass::D, "run recipes").is_err());
    let op = Caller::from_arg(Some(json!({ "actor": "o", "role": "operator" })));
    assert!(op.is_api(), "a caller value is always an API caller");
    assert!(op.require_class(CapClass::L, "x").is_ok());
    assert!(op.require_class(CapClass::C, "x").is_err());
}

#[test]
fn loopback_urls_only() {
    assert!(is_loopback_url("http://127.0.0.1:8081/health"));
    assert!(is_loopback_url("http://localhost:18099/completion"));
    assert!(is_loopback_url("http://[::1]:80/"));
    assert!(!is_loopback_url("http://10.0.0.2:8081/"));
    assert!(!is_loopback_url("http://127.0.0.1.evil.com/"));
    assert!(!is_loopback_url("file:///etc/passwd"));
}

#[test]
fn allowlist_matches_dirs_globs_and_path_names() {
    let dir = std::env::temp_dir().join(format!("console-allow-{}", std::process::id()));
    let llama = dir.join("llamacpp");
    std::fs::create_dir_all(&llama).unwrap();
    let bench = llama.join(if cfg!(windows) {
        "llama-bench.exe"
    } else {
        "llama-bench"
    });
    std::fs::write(&bench, b"").unwrap();
    let other = dir.join("evil").join(bench.file_name().unwrap());
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::write(&other, b"").unwrap();
    let mut ctx = manager_diag::TemplateContext::new();
    ctx.set("llamacpp_bin", llama.to_string_lossy().into_owned());
    let entries = vec!["{llamacpp_bin}/*".to_string(), "nvidia-smi".to_string()];
    let env: BTreeMap<String, String> = [(
        "PATH".to_string(),
        other.parent().unwrap().to_string_lossy().into_owned(),
    )]
    .into();
    assert_eq!(
        allowlisted(&bench, &entries, &ctx, &env).as_deref(),
        Some("{llamacpp_bin}/*")
    );
    // Same file name elsewhere: not allowlisted by the directory entry,
    // and a bare-name entry only matches what PATH resolves.
    assert!(allowlisted(&other, &entries, &ctx, &env).is_none());
    let by_name = vec![bench.file_stem().unwrap().to_string_lossy().into_owned()];
    assert!(allowlisted(&other, &by_name, &ctx, &env).is_some());
    assert!(allowlisted(&PathBuf::from(&bench), &by_name, &ctx, &env).is_none());
    let glob = vec![format!("{}/llama-*", llama.to_string_lossy())];
    assert!(allowlisted(&bench, &glob, &ctx, &env).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn display_quotes_but_never_executes() {
    let c = display_command(
        "C:/x y/llama-bench.exe",
        &["-m".into(), "a b.gguf".into(), "-p".into(), "512".into()],
    );
    assert_eq!(c, "\"C:/x y/llama-bench.exe\" -m \"a b.gguf\" -p 512");
}

#[test]
fn synthetic_prompt_is_roughly_the_requested_size() {
    let p = synthetic_prompt(8192);
    let words = p.split_whitespace().count();
    assert!((7000..10000).contains(&words), "{words} words");
    assert_eq!(p, synthetic_prompt(8192), "deterministic");
}

//! F1 follow-up (docs/code-review-v0.1.60.md): the gateway venv
//! install is hash-locked when the checked-in lock covers the LiteLLM
//! pin, and falls back to the plain pinned install — with a "not
//! hash-verified" warning — when it does not.

use super::*;
use crate::gateway_lock::{set_test_lock_override, tests::fixture_lock};

/// Restores the embedded lock when a test ends (even on panic).
struct LockOverride;

impl LockOverride {
    fn set(lock: String) -> Self {
        set_test_lock_override(Some(lock));
        Self
    }
}

impl Drop for LockOverride {
    fn drop(&mut self) {
        set_test_lock_override(None);
    }
}

fn ctx_with_pin(ops: Arc<FakeSystemOps>, pin: &str) -> WizardContext {
    let mut cfg = test_config_with_models();
    cfg.updates
        .components
        .get_mut("litellm")
        .expect("default config carries the litellm component")
        .pinned_version = Some(pin.to_string());
    WizardContext::new(cfg, PathBuf::from("/data"), RunMode::Install, ops)
}

fn quick_step() -> StartGateway {
    StartGateway {
        health_retries: 3,
        health_interval_ms: 1,
        db_sustain_ms: 0,
    }
}

fn lock_path() -> PathBuf {
    PathBuf::from("/data")
        .join("python")
        .join(crate::gateway_lock::GATEWAY_LOCK_FILE_NAME)
}

fn has(argv: &[String], s: &str) -> bool {
    argv.iter().any(|a| a == s)
}

#[test]
fn pip_command_is_hash_locked_when_the_lock_covers_the_pin() {
    let _lock = LockOverride::set(fixture_lock(manager_config::DEFAULT_LITELLM_PIN));
    let step = StartGateway::default();
    let ctx = ctx_with(Arc::new(FakeSystemOps::new()), RunMode::Install);
    assert_eq!(step.hash_lock_mode(&ctx), Ok(()));
    let cmd = step.pip_command(&ctx, std::path::Path::new("uv"));
    assert_eq!(
        &cmd.argv[1..],
        &[
            "pip".to_string(),
            "install".to_string(),
            "--python".to_string(),
            step.venv_python(&ctx).display().to_string(),
            "--require-hashes".to_string(),
            "-r".to_string(),
            lock_path().display().to_string(),
        ]
    );
    // The bare requirements are not mixed in: everything comes from the lock.
    assert!(
        !cmd.argv.iter().any(|a| a.starts_with("litellm")),
        "{cmd:?}"
    );
}

#[test]
fn pip_command_falls_back_unhashed_for_an_uncovered_operator_pin() {
    let _lock = LockOverride::set(fixture_lock(manager_config::DEFAULT_LITELLM_PIN));
    let step = StartGateway::default();
    let ctx = ctx_with_pin(Arc::new(FakeSystemOps::new()), "1.104.2");
    let warning = step.hash_lock_mode(&ctx).unwrap_err();
    assert!(warning.contains("not hash-verified"), "{warning}");
    assert!(
        warning.contains(&format!(
            "covers LiteLLM {}, not the pinned 1.104.2",
            manager_config::DEFAULT_LITELLM_PIN
        )),
        "{warning}"
    );
    let cmd = step.pip_command(&ctx, std::path::Path::new("uv"));
    assert!(!has(&cmd.argv, "--require-hashes"), "{cmd:?}");
    assert!(has(&cmd.argv, "litellm[proxy]==1.104.2"), "{cmd:?}");
    assert!(
        has(
            &cmd.argv,
            &format!("prisma=={}", crate::PRISMA_PINNED_VERSION)
        ),
        "{cmd:?}"
    );
    assert!(has(&cmd.argv, "prometheus-client"), "{cmd:?}");
}

/// Without an override the embedded lock decides: while it is the
/// ungenerated placeholder every install is the reported fallback.
#[test]
fn embedded_lock_decides_the_default_install() {
    let step = StartGateway::default();
    let ctx = ctx_with(Arc::new(FakeSystemOps::new()), RunMode::Install);
    let cmd = step.pip_command(&ctx, std::path::Path::new("uv"));
    let generated = !crate::gateway_lock::parse_requirements_lock(
        crate::gateway_lock::GATEWAY_REQUIREMENTS_LOCK,
    )
    .unwrap()
    .is_empty();
    assert_eq!(has(&cmd.argv, "--require-hashes"), generated, "{cmd:?}");
    assert_eq!(step.hash_lock_mode(&ctx).is_ok(), generated);
}

#[test]
fn prisma_repair_uses_the_lock_only_when_it_covers_the_pin() {
    let _lock = LockOverride::set(fixture_lock(manager_config::DEFAULT_LITELLM_PIN));
    let step = StartGateway::default();
    let uv = std::path::Path::new("uv");
    let ctx = ctx_with(Arc::new(FakeSystemOps::new()), RunMode::Install);
    let hashed = step.prisma_pin_command(&ctx, uv);
    assert!(has(&hashed.argv, "--require-hashes"), "{hashed:?}");
    assert!(has(&hashed.argv, "--reinstall-package"), "{hashed:?}");
    assert!(has(&hashed.argv, &lock_path().display().to_string()));
    let ctx = ctx_with_pin(Arc::new(FakeSystemOps::new()), "1.104.2");
    let plain = step.prisma_pin_command(&ctx, uv);
    assert!(!has(&plain.argv, "--require-hashes"), "{plain:?}");
    assert!(has(
        &plain.argv,
        &format!("prisma=={}", crate::PRISMA_PINNED_VERSION)
    ));
}

#[tokio::test]
async fn start_gateway_installs_from_the_hash_lock() {
    let lock = fixture_lock(manager_config::DEFAULT_LITELLM_PIN);
    let _lock = LockOverride::set(lock.clone());
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    let out = quick_step().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(!out.message.contains("not hash-verified"), "{out:?}");
    // The embedded lock was materialized for `-r`, byte for byte.
    assert_eq!(ops.file_bytes(&lock_path()), Some(lock.into_bytes()));
    let pip = ops
        .executed_commands()
        .into_iter()
        .find(|a| has(a, "install"))
        .expect("pip install ran");
    assert!(has(&pip, "--require-hashes"), "{pip:?}");
    assert!(has(&pip, &lock_path().display().to_string()), "{pip:?}");
    assert!(
        out.log_tail.iter().any(|l| l.contains("--require-hashes")),
        "{out:?}"
    );
}

#[tokio::test]
async fn start_gateway_reports_an_unverified_install_for_an_uncovered_pin() {
    let _lock = LockOverride::set(fixture_lock(manager_config::DEFAULT_LITELLM_PIN));
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let ctx = ctx_with_pin(ops.clone(), "1.104.2");
    let out = quick_step().run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    // Never silent: the outcome message and the log both say it.
    assert!(out.message.contains("not hash-verified"), "{out:?}");
    assert!(
        out.log_tail
            .iter()
            .any(|l| l.starts_with("warning:") && l.contains("not hash-verified")),
        "{out:?}"
    );
    assert!(ops.file_bytes(&lock_path()).is_none());
    let pip = ops
        .executed_commands()
        .into_iter()
        .find(|a| has(a, "install"))
        .expect("pip install ran");
    assert!(!has(&pip, "--require-hashes"), "{pip:?}");
    assert!(has(&pip, "litellm[proxy]==1.104.2"), "{pip:?}");
}

/// A reused, healthy venv installs nothing, so there is nothing to
/// verify and no warning.
#[tokio::test]
async fn reused_venv_carries_no_hash_warning() {
    let _lock = LockOverride::set(fixture_lock("0.0.1"));
    let ops = Arc::new(FakeSystemOps::new());
    with_bundled_uv(&ops);
    ops.with_http_get(
        "http://127.0.0.1:4000/v1/models",
        FakeSystemOps::http_ok(r#"{"data":[]}"#),
    );
    let step = quick_step();
    let ctx = ctx_with(ops.clone(), RunMode::Install);
    ops.with_file(step.venv_python(&ctx), Vec::new());
    let probe = step.version_probe_command(&ctx);
    let argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
    ops.with_command(
        &argv,
        CommandOutput {
            exit_code: 0,
            stdout: format!("{}\n", crate::GATEWAY_PYTHON_VERSION),
            stderr: String::new(),
        },
    );
    let probe = step.litellm_probe_command(&ctx);
    let argv: Vec<&str> = probe.argv.iter().map(String::as_str).collect();
    ops.with_command(
        &argv,
        CommandOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        },
    );
    let out = step.run(&ctx).await.unwrap();
    assert_eq!(out.state, StepState::Done, "{out:?}");
    assert!(
        !ops.executed_commands().iter().any(|a| has(a, "install")),
        "nothing installed"
    );
    assert!(!out.message.contains("not hash-verified"), "{out:?}");
}

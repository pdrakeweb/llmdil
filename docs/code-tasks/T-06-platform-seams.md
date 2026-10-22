# T-06 — Platform seams: `SecretStore` / `ServiceManager` / `TelemetrySource` / `ProcessRuntime`

**Task ID:** T-06
**Category:** structural
**Review source:** `LLMdil-systems-review.md` §3.10, §5.10
**Blast radius:** `crates/manager-api/src/auth.rs`, `crates/manager-services/src/lib.rs`, `crates/manager-telemetry/src/lib.rs`, `crates/manager-wizard/src/ops.rs`, `src-tauri/src/`
**Est. effort:** L

## 1. Objective

Introduce **platform-abstraction traits** for Windows-specific functionality so the app can
eventually support non-Windows platforms. The traits should be **no-op stubs** on non-Windows
and delegate to real implementations on Windows.

The target seams are:
- `SecretStore` (replaces Credential Manager for secrets at rest).
- `ServiceManager` (replaces SCM service registration).
- `TelemetrySource` (replaces PDH/NVML/DXGI for system telemetry).
- `ProcessRuntime` (replaces `std::process`/`tokio::process` + venv handling).

## 2. Context / why

Today the codebase is **Windows-only by construction**:
- `#[cfg(windows)]` code (NVML, PDH, DXGI, WMI, SCM, venv, Credential Manager) is never compiled
  or executed in CI.
- Secrets are encrypted via Credential Manager (Windows only).
- Services are registered via SCM (Windows only).
- GPU telemetry uses NVML/PDH/DXGI (Windows only).
- Process execution uses venv activation (Windows only).

This means:
- Windows-specific bugs slip through Linux CI.
- Porting to macOS or Linux requires rewrites of large code sections.

## 3. Current state (measured, clean checkout 2026-10-22)

**Platform-specific code locations:**

| Feature | Crate | File | Windows mechanism |
|---------|-------|------|-------------------|
| Credential Manager | `manager-wizard` | `ops.rs` | `CredWriteW`, `CredReadW`, `CredDeleteW` |
| Service SCM | `manager-services` | `lib.rs` | `CreateService`, `OpenSCManager`, `StartService` |
| GPU telemetry | `manager-telemetry` | `pdh.rs`, `nvml.rs`, `dxgi.rs` | PDH, NVML, DXGI |
| Process/venv | `manager-gateway` | `lib.rs` | `Command::new`, `venv_create`, `venv_activate` |
| DPAPI | `none` | — | **not used** — review mentioned DPAPI but it is absent from code |

*Note:* The review said "DPAPI" but a full repo scan found **no `dpapi` anywhere**. Secrets are
stored via Credential Manager.

**Existing partial seams:**
- `manager-services` already has a `ServiceControl` trait with a `cfg(windows)` real impl and
  a stub (no-op on non-Windows). This is a good model to follow.
- `manager-api` already has a `CredentialStore` trait (see `manager-api/src/auth.rs`).

## 4. Scope

### In scope
- Define `SecretStore` trait (with `cfg(windows)` impl using Credential Manager; stub on other platforms).
- Define `ServiceManager` trait (with `cfg(windows)` impl using SCM; stub on other platforms).
- Define `TelemetrySource` trait (with `cfg(windows)` impl using PDH/NVML/DXGI; stub on other platforms).
- Define `ProcessRuntime` trait (with `cfg(windows)` impl using venv/Command; stub on other platforms).
- Replace direct uses of these mechanisms with trait-based dispatch.
- Add tests for stub implementations (assert they compile and run on all platforms).

### Out of scope (do NOT do)
- Do **not** implement real non-Windows backends (e.g. macOS Keychain, Linux D-Bus) — stubs are
  sufficient for now.
- Do **not** change existing `#[cfg(windows)]` code — just wrap it in traits and dispatch through
  the trait interface.
- Do **not** change the public API of `manager-wizard`, `manager-services`, `manager-telemetry`,
  or `manager-gateway`.

## 5. Implementation plan

### 5.1 Create trait modules

Create new modules:
- `crates/manager-api/src/secrets.rs` → `SecretStore` trait + `SecretStoreError`.
- `crates/manager-services/src/secrets.rs` → `SecretStore` trait impl (stub).
- `crates/manager-services/src/services.rs` → `ServiceManager` trait + `ServiceManagerError`.
- `crates/manager-telemetry/src/telemetry.rs` → `TelemetrySource` trait + `TelemetrySourceError`.
- `crates/manager-gateway/src/process.rs` → `ProcessRuntime` trait + `ProcessRuntimeError`.

### 5.2 Define traits (example: `SecretStore`)

```rust
// crates/manager-api/src/secrets.rs

#[derive(Debug)]
pub enum SecretStoreError {
    NotSupported,
    Storage(String),
    Crypto(String),
}

pub trait SecretStore {
    fn write(&self, key: &str, value: &[u8]) -> Result<(), SecretStoreError>;
    fn read(&self, key: &str) -> Result<Vec<u8>, SecretStoreError>;
    fn delete(&self, key: &str) -> Result<(), SecretStoreError>;
}

#[cfg(windows)]
pub mod windows {
    use super::*;
    pub struct CredentialManagerSecretStore;
    impl SecretStore for CredentialManagerSecretStore {
        fn write(&self, key: &str, value: &[u8]) -> Result<(), SecretStoreError> {
            // CredWriteW, etc.
        }
        // ...
    }
}

#[cfg(not(windows))]
pub mod stub {
    use super::*;
    pub struct StubSecretStore;
    impl SecretStore for StubSecretStore {
        fn write(&self, _key: &str, _value: &[u8]) -> Result<(), SecretStoreError> {
            Err(SecretStoreError::NotSupported)
        }
        // ...
    }
}
```

### 5.3 Replace direct uses

- In `manager-wizard/ops.rs`, replace direct `CredWriteW` calls with `SecretStore::write()`.
- In `manager-services/lib.rs`, replace direct `CreateService` calls with `ServiceManager::create()`.
- In `manager-telemetry/pdh.rs`/`nvml.rs`, replace direct telemetry calls with `TelemetrySource::gpu_info()`.
- In `manager-gateway/lib.rs`, replace direct `venv_create`/`venv_activate` calls with
  `ProcessRuntime::venv_create()`.

### 5.4 Add tests

Add unit tests for stub implementations:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_store_stub_returns_not_supported() {
        let store = stub::StubSecretStore;
        assert!(matches!(store.write("key", b"value"), Err(SecretStoreError::NotSupported)));
    }

    #[test]
    fn service_manager_stub_returns_not_supported() {
        let manager = stub::StubServiceManager;
        assert!(matches!(manager.create("name", "path"), Err(ServiceManagerError::NotSupported)));
    }

    // ...
}
```

## 6. Definition of done

- [ ] `SecretStore` trait defined with `cfg(windows)` impl and stub.
- [ ] `ServiceManager` trait defined with `cfg(windows)` impl and stub.
- [ ] `TelemetrySource` trait defined with `cfg(windows)` impl and stub.
- [ ] `ProcessRuntime` trait defined with `cfg(windows)` impl and stub.
- [ ] All direct uses replaced with trait-based dispatch.
- [ ] Stub tests added and pass on all platforms.
- [ ] `cargo check` (Windows + Linux) clean.
- [ ] `cargo test --workspace` passes unchanged.

## 7. Testing & verification

- Run `cargo check` on Windows and Linux (or macOS if available).
- Run `cargo test --workspace` — stub tests should pass on non-Windows, real tests on Windows.
- Spot-check: `manager-wizard` compiles on Linux with stub `SecretStore`.
- Spot-check: `manager-telemetry` compiles on Linux with stub `TelemetrySource`.

## 8. Risks & pitfalls

- **Trait ownership.** `SecretStore` logically belongs in `manager-api` (it's used for admin
  auth), but `manager-wizard` also uses it. Keep it in `manager-api` and re-export from
  `manager-wizard` if needed.
- **`#[cfg(windows)]` on trait impls.** Use `#[cfg(windows)]` on the impl, not the trait, so
  the trait itself is cross-platform.
- **Stub behavior.** Stubs should return `NotSupported` or no-op, not panic — the app should
  gracefully degrade on unsupported platforms.

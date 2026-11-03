# T-04 — Make CI a real gate (Windows job, clippy/fmt, UI build, version-drift assert, pre-release smoke)

**Task ID:** T-04
**Category:** structural
**Review source:** `LLMdil-systems-review.md` §3.9, §5.4
**Blast radius:** `.github/workflows/` (only `ci.yml`, `release.yml`, `install-test.yml`)
**Est. effort:** M

## 1. Objective

Upgrade CI from a **Linux-only workspace test runner** to a **Windows-first gate** that:
- Compiles on Windows (no `--no-default-features` hack).
- Enforces `clippy -D warnings` and `fmt` on all crates and `src-tauri`.
- Builds and tests the UI (`npm ci`, `npm run build`, optionally `npm test`).
- Asserts version-drift consistency (README vs `Cargo.toml` vs `ui/package.json` vs `tauri.conf.json`).
- Runs a pre-release smoke test (e.g. `cargo run` on Windows and basic health checks).

## 2. Context / why

Current CI (`ci.yml`) runs on `ubuntu-latest` with only `cargo test --workspace`. It:
- Does not compile Windows-specific code (`#[cfg(windows)]`).
- Does not lint (`clippy`) or format (`fmt`) code.
- Does not build or test the UI.
- Does not check version consistency.
- Runs *after* release (`install-test.yml`) instead of as a gate.

This means PRs can merge with broken Windows code, formatting violations, or version drift.

## 3. Current state (measured, clean checkout 2026-10-22)

**`.github/workflows/ci.yml`** (130 lines):

| Section | Content |
|---------|---------|
| `name: CI` | default |
| `on: [push, pull_request]` | default |
| `jobs:` | `test` (only job) |
| `test:` | runs on `ubuntu-latest` |
| `steps:` | `checkout`, `cache`, `cargo test --workspace` |

**`.github/workflows/release.yml`** (160 lines):

| Section | Content |
|---------|---------|
| `name: Release` | default |
| `on: push: tags: ["v*"]` | default |
| `jobs:` | `release` (Windows NSIS via `tauri-action`) |
| `release:` | no clippy/fmt gate, no Windows compile job, no UI build test |

**`.github/workflows/install-test.yml`** (95 lines):

| Section | Content |
|---------|---------|
| `name: Install test` | default |
| `on: workflow_dispatch` | manual, *post-release* |
| `jobs:` | `install-test` (no CI gate) |

**`.github/workflows/prometheus-gates.yml`** (170 lines):

| Section | Content |
|---------|---------|
| `name: Prometheus gates` | self-hosted runner |
| `on: workflow_dispatch` | manual |
| `jobs:` | `prometheus` (Windows, `cargo fmt`, `cargo clippy -D warnings`, `cargo test`, `npm ci`, `npm run build`, `cargo run` smoke) |
| `prometheus:` | **already** implements all gates — but is manual, not a PR gate |

## 4. Scope

### In scope
- Add a **Windows compile job** to `ci.yml` (or rename `prometheus-gates.yml` to ` gates.yml`
  and make it run on PR).
- Enforce `clippy -D warnings` and `cargo fmt` on all crates and `src-tauri`.
- Build and test the UI (`npm ci`, `npm run build`, optionally `npm test`).
- Assert version consistency (README vs `Cargo.toml` vs `ui/package.json` vs `tauri.conf.json`).
- Run a **pre-release smoke test** (e.g. `cargo run` on Windows and basic health checks).
- Update `release.yml` to include the CI gates *before* the release step.

### Out of scope (do NOT do)
- Do **not** change the existing `prometheus-gates.yml` (keep it for manual runs).
- Do **not** change `install-test.yml` (keep it as post-release verification).
- Do **not** change the `Cargo.toml` or `ui/package.json` version strings — just add an
  assertion that they match.
- Do **not** change the NSIS packaging or updater logic.

## 5. Implementation plan

### 5.1 `ci.yml` → `gates.yml` (or `ci.yml` with multiple jobs)

Rename `ci.yml` to `gates.yml` and add:

| Job | Runner | Content |
|-----|--------|---------|
| `windows-compile` | `windows-latest` | `cargo check --workspace`, `cargo check --package src-tauri` |
| `fmt` | `ubuntu-latest` | `cargo fmt --check` |
| `clippy` | `ubuntu-latest` | `cargo clippy --workspace -- -D warnings` |
| `ui-build` | `ubuntu-latest` | `npm ci`, `npm run build` |
| `version-check` | `ubuntu-latest` | assert versions match (see 5.2) |
| `smoke` | `windows-latest` (or reuse `prometheus` runner) | `cargo run --package src-tauri` + basic health checks |

### 5.2 Version-drift assertion

Create a small script (`tools/ci-check-version.sh` or `tools/ci-check-version.ps1`) that:

1. Reads `Cargo.toml` workspace version.
2. Reads `ui/package.json` version.
3. Reads `src-tauri/tauri.conf.json` version.
4. Reads `README.md` version (first `v0.x.y` match).
5. Fails if any mismatch.

In `ci.yml`/`gates.yml`, run:
```yaml
- name: Version check
  run: cargo run --manifest-path tools/ci-check-version/Cargo.toml
```

or a PowerShell script:
```yaml
- name: Version check
  shell: pwsh
  run: .\tools\ci-check-version.ps1
```

### 5.3 Smoke test

In `smoke` job (Windows), after `cargo run`:
- Check `http://127.0.0.1:3001/health` returns `200`.
- Check `http://127.0.0.1:3001/api/v1/version` returns the correct version.
- Stop the app.

### 5.4 Update `release.yml`

Move `prometheus` gates (or `gates.yml` jobs) *before* `tauri-action`:
- Run all gates first.
- On success, tag and push → `tauri-action`.
- On failure, abort.

## 6. Definition of done

- [ ] `ci.yml` renamed to `gates.yml` (or new `ci.yml` created) with Windows compile job.
- [ ] `fmt` and `clippy -D warnings` gates added (Ubuntu runner).
- [ ] UI build gate added (`npm ci`, `npm run build`).
- [ ] Version-drift assertion added (fails CI if versions differ).
- [ ] Smoke test added (`cargo run` + health check).
- [ ] `release.yml` runs gates *before* NSIS packaging.
- [ ] `prometheus-gates.yml` kept unchanged (manual runs only).
- [ ] `install-test.yml` kept unchanged (post-release verification only).

## 7. Testing & verification

- Push to a feature branch and verify CI jobs run.
- Check that a Windows PR fails if `#[cfg(windows)]` code has a syntax error.
- Check that a PR fails if `cargo fmt` or `cargo clippy -D warnings` fails.
- Check that a PR fails if UI build fails.
- Check that a PR fails if version drift is detected.

## 8. Risks & pitfalls

- **Windows runner availability.** GitHub-hosted Windows runners are available but may be slower
  than Linux. Consider adding a `concurrency` group to avoid duplicate runs on force-push.
- **`prometheus-gates.yml` manual runner.** If the self-hosted "Prometheus" runner is down,
  manual gates will fail. This is acceptable — it's a manual trigger anyway.
- **Version-drift assertion.** If you add more version sources (e.g. `CHANGELOG.md`), update
  the script to include them.

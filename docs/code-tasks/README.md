# LLMdil — Structural Code Tasks

This folder contains **discrete, independently-implementable code tasks** derived from the
systems review (`docs/../LLMdil-systems-review.md`, i.e. `LLMdil-systems-review.md` at the
scratch root). Each file is **self-contained**: it carries the context an independent coding
agent needs to implement the task **without materially altering the functionality or
correctness of the software**.

## The universal invariant (applies to EVERY task)

> **No behavioral change.** Each task must leave observable behavior, the public API surface,
> and — where applicable — the operation registry (names, argument names/types, HTTP routes,
> capability classes), the wire formats, and the existing test suite **semantically identical**.
> Where a task moves code, it moves **bodies only** — function signatures, exported names,
> serialization field names, and route paths stay exactly as they are.
>
> A task is **verified successful** when: (1) it compiles on Windows (`cargo check` /
> `cargo build`), (2) the full test suite passes unchanged
> (`cargo test --workspace` with the same feature flags the CI uses), and (3) the specific
> checks listed in the task's own *Definition of done* pass. No test may be renamed, deleted,
> or weakened. New tests may only be *added*.

## Task index

| ID | File | Title | Category | Review § | Est. effort |
|----|------|-------|----------|----------|-------------|
| T-01 | `T-01-registry-typed-envelopes.md` | Extend registry codegen → typed request/response structs, generated TS types, per-op JSON Schema | structural | 5.1 | L |
| T-02 | `T-02-godfile-decomposition.md` | Decompose god files — umbrella + shared rules + per-crate table | decomposition | 5.2 | umbrella |
| T-02a | `T-02a-decompose-manager-app.md` | `crates/manager-app/src/lib.rs` (15,077) + `optimize.rs` (3,322) | decomposition | 5.2 | XL |
| T-02b | `T-02b-decompose-manager-wizard.md` | `crates/manager-wizard/src/steps.rs` (12,596) + `lib.rs` (3,113) + `ops.rs` (2,017) | decomposition | 5.2 | XL |
| T-02c | `T-02c-decompose-manager-supervisor.md` | `crates/manager-supervisor/src/lib.rs` (4,027) | decomposition | 5.2 | L |
| T-02d | `T-02d-decompose-manager-config.md` | `crates/manager-config/src/lib.rs` (3,795) | decomposition | 5.2 | L |
| T-02e | `T-02e-decompose-manager-gateway.md` | `crates/manager-gateway/src/lib.rs` (2,996) | decomposition | 5.2 | M |
| T-02f | `T-02f-decompose-manager-optimize.md` | `crates/manager-optimize/src/parse.rs` (2,653) | decomposition | 5.2 | M |
| T-02g | `T-02g-decompose-manager-models.md` | `crates/manager-models/src/lib.rs` (2,303) | decomposition | 5.2 | M |
| T-02h | `T-02h-decompose-ui.md` | `ui/src/views/static.ts` (8,816) + `api.ts` (3,868) | decomposition | 5.2 | L |
| T-03 | `T-03-appstate-subsystem-state.md` | Replace the `AppState` ad-hoc lock pile with per-subsystem state objects | structural | 5.3 | XL |
| T-04 | `T-04-ci-gates.md` | Make CI a real gate (Windows job, clippy/fmt, UI build, version-drift assert, pre-release smoke) | structural | 5.7 | M |
| T-05 | `T-05-logging-pipeline.md` | One logging pipeline: init `tracing`, `task_id` spans, feed the diagnostics export | structural | 5.6 | M |
| T-06 | `T-06-platform-seams.md` | Platform seams: `SecretStore` / `ServiceManager` / `TelemetrySource` / `ProcessRuntime` | structural | 5.10 | L |

## Dependencies & recommended ordering

The tasks are designed to be **independently dispatchable**. Where two tasks touch the same
file, sequence them to avoid edit conflicts (each still verifies on its own):

- **T-02a** (decompose `manager-app/src/lib.rs`) and **T-03** (rework `AppState`) both edit
  `crates/manager-app/src/lib.rs`. Do **one, land it, then the other** — or merge into a single
  execution. Each is independently verifiable either way.
- **T-05** (logging) adds initialization to `src-tauri/src/main.rs` and a `task_id` span in
  `manager-app/src/lib.rs`. It is otherwise independent; if run after T-02a/T-03 the anchors
  shift, so re-read the file.
- **T-01** (registry codegen) edits the macro in `crates/manager-app/src/registry.rs` and
  `src-tauri/src/main.rs`. Independent of the decomposition tasks.
- **T-02b…T-02h** are different crates/modules and can run fully in parallel with each other.
  Every Rust/TS file > 2,000 lines is covered by exactly one subtask (verified by a full repo
  scan on 2026-10-22).
- **T-04** (CI) and **T-06** (platform seams) are independent of everything else.

**Suggested first wave (lowest risk, highest leverage, parallel-safe):** T-04, T-05, T-06,
T-02c, T-02d, T-02e, T-02f, T-02g. **Second wave:** T-01, T-02a → then T-03, T-02b, T-02h.

## How to dispatch a task to an independent coding agent

1. Pass the agent **only** the single task file (it is self-contained) plus the repo checkout.
2. The agent must read the task's *Current state* section to ground itself in the real code,
   then implement per *Implementation plan*.
3. The agent **must not** touch files outside the task's *Blast radius* unless the task says so.
4. Success = the task's *Definition of done* checklist is fully green.

## Measurement baseline (use for before/after verification)

Measured on a clean checkout (2026-10-22):

| Item | Value |
|------|-------|
| Rust files / lines | 75 `.rs` / 90,671 lines |
| UI (TypeScript) lines | 18,714 |
| Tests (`#[test]`/`#[tokio::test]`) | 1,027 total |
| `OP_SPECS.len()` | 130 |
| `AppState` fields | 32 |
| `std::fs::` vs `tokio::fs::` | 418 vs 30 |
| `unwrap()` / `.expect(` / `panic!` / `unsafe` | 1,357 / 368 / 36 / 85 |
| `tracing::` sites / subscriber init | ~20 / **0** |
| `setInterval` (UI) / `EventSource` (UI) | 25 / 0 |

| God file | Lines (measured) |
|----------|-------------------|
| `crates/manager-app/src/lib.rs` | 15,077 |
| `crates/manager-wizard/src/steps.rs` | 12,596 |
| `ui/src/views/static.ts` | 8,816 |
| `crates/manager-supervisor/src/lib.rs` | 4,027 |
| `ui/src/api.ts` | 3,868 |
| `crates/manager-config/src/lib.rs` | 3,795 |
| `crates/manager-app/src/optimize.rs` | 3,322 |
| `crates/manager-wizard/src/lib.rs` | 3,113 |
| `crates/manager-gateway/src/lib.rs` | 2,996 |
| `crates/manager-optimize/src/parse.rs` | 2,653 |
| `crates/manager-models/src/lib.rs` | 2,303 |
| `crates/manager-wizard/src/ops.rs` | 2,017 |

**Decomposition target: no source file > 2,000 lines** (from review §5.2). Files already
under that limit (e.g. `manager-winml` 881, `manager-telemetry` modules, `manager-services`)
require **no** decomposition task.

## Key facts a task agent must not break (cross-task)

- `src-tauri/tauri.conf.json` → `app.withGlobalTauri: true` **must stay true**. The UI detects
  Tauri via `window.__TAURI__.core.invoke`; removing it silently degrades every view to mock data.
- The operation registry is the single source of truth. Any change must keep
  `OP_SPECS.len() == 130` (until a task explicitly adds/removes ops) and keep every existing
  `name`, argument, route, and `CapClass` unchanged.
- The registry's drift tests in `crates/manager-app/src/registry.rs`
  (`names_and_routes_are_unique`, `every_spec_round_trips_through_lookup`,
  `role_matrix_matches_plan`, `frontend_invocations_are_all_registered`,
  `dispatch_validates_arguments_before_running`, `dispatch_reaches_the_operation`) must keep
  passing.
- Admin-API security posture (loopback-only default, argon2, TOTP, lockout, rustls self-signed
  TLS, audit log) must not be weakened by any task.
- Windows-only code (`#[cfg(windows)]`: NVML, PDH, DXGI, WMI, SCM, venv, Credential Manager)
  **must be verified on a Windows toolchain** — Linux CI does not compile it.

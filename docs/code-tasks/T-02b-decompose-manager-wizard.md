# T-02b — Decompose `crates/manager-wizard/src/steps.rs`, `lib.rs`, and `ops.rs`

**Task ID:** T-02b (subtask of T-02)
**Category:** decomposition
**Blast radius:** `crates/manager-wizard/src/`
**Est. effort:** XL

## 1. Objective

Reduce `steps.rs` (12,596 lines), `lib.rs` (3,113 lines), and `ops.rs` (2,017 lines) to
**≤ 2,000 lines each** by moving their function/type definitions into cohesive submodules,
preserving all signatures, names, and public API via `pub use` re-exports.

## 2. Current state (measured, clean checkout 2026-10-22)

| File | Lines | Key contents |
|------|-------|--------------|
| `crates/manager-wizard/src/steps.rs` | 12,596 | 10 install-step implementations (`step_one` → `step_ten`) |
| `crates/manager-wizard/src/lib.rs` | 3,113 | `Wizard`, `WizardState`, `Step`, `SystemOps`, `fake.rs`, `credential_write` |
| `crates/manager-wizard/src/ops.rs` | 2,017 | `Ops`, `OpsError`, credential helpers, venv helpers |

**`steps.rs` module map (by step function):**

| Function | Lines | Content |
|----------|-------|---------|
| `step_one` | ~1,200 | `StepDefinition`, `StepContext`, `StepResult`, `StepOutcome` |
| `step_two` | ~1,100 | GPU detection (`nvml`, `pdh`, `dxgi`), VRAM check |
| `step_three` | ~1,200 | venv creation (`venv_create`, `venv_install`) |
| `step_four` | ~1,100 | venv activation (`venv_activate`, `venv_path`) |
| `step_five` | ~1,100 | model download (`download_model`, `download_cancel`, `download_progress`) |
| `step_six` | ~1,000 | GPU optimization (`gpu_optimize`, `gpu_optimize_abort`, `gpu_optimize_history`) |
| `step_seven` | ~1,000 | venv model install (`venv_model_install`) |
| `step_eight` | ~1,000 | service registration (`service_create`, `service_register`, `service_start`) |
| `step_nine` | ~1,000 | admin user setup (`admin_init`, `admin_login`, `admin_set_role`) |
| `step_ten` | ~1,000 | finalization (`finalize`, `cleanup`) |
| `run_step` | ~1,500 | orchestrator (`run_step`, `run_step_impl`, `StepRunner`) |
| `default_steps` | ~1,500 | `StepDefinition` array + metadata |
| helpers | ~2,000 | `StepError`, `StepKind`, `StepStatus`, `StepOutcome`, `StepProgress` |

**`lib.rs` module map:**

| Section | Lines | Content |
|---------|-------|---------|
| `mod system` | inline | `SystemOps`, `SystemOpsError`, `SystemOpsImpl` |
| `mod ops` | inline | `Ops`, `OpsError`, credential helpers |
| `mod fake` | inline | `FakeSystemOps`, `FakeCredentialStore` |
| `mod steps` | inline | `steps.rs` contents |
| `mod wizard` | inline | `Wizard`, `WizardState`, `WizardError`, `WizardAction`, `WizardEvent` |
| tests | inline | tests |

**`ops.rs` module map:**

| Section | Lines | Content |
|---------|-------|---------|
| `Ops` | ~300 | `Ops`, `OpsError`, `CredentialStore`, `Credential` |
| `credential_write` | ~250 | Windows Credential Manager (`CredWriteW`, `CredReadW`, `CredDeleteW`) |
| `credential_delete` | ~150 | Credential Manager delete |
| `credential_list` | ~100 | Credential Manager list |
| `venv_create` | ~300 | venv creation helpers |
| `venv_install` | ~300 | venv package installation |
| `service_create` | ~200 | SCM service registration |
| `service_register` | ~200 | service registration helpers |
| `service_start` | ~150 | service start helpers |

## 3. Decomposition plan (concrete, module-by-module)

### 3.1 New modules

| Module | Target lines | Content | Re-export from `lib.rs` |
|--------|-------------|---------|------------------------|
| `steps/` (directory) | ~12,596 → split into ~5 files | per-step or domain grouping | `pub use steps::one::*;` etc. |
| `steps/one.rs` | ~1,200 | `StepDefinition`, `StepContext`, `StepResult`, `StepOutcome` |
| `steps/two.rs` | ~1,100 | GPU detection (`nvml`, `pdh`, `dxgi`), VRAM check |
| `steps/three.rs` | ~1,200 | venv creation (`venv_create`, `venv_install`) |
| `steps/four.rs` | ~1,100 | venv activation (`venv_activate`, `venv_path`) |
| `steps/five.rs` | ~1,100 | model download (`download_model`, `download_cancel`, `download_progress`) |
| `steps/six.rs` | ~1,000 | GPU optimization (`gpu_optimize`, `gpu_optimize_abort`, `gpu_optimize_history`) |
| `steps/seven.rs` | ~1,000 | venv model install (`venv_model_install`) |
| `steps/eight.rs` | ~1,000 | service registration (`service_create`, `service_register`, `service_start`) |
| `steps/nine.rs` | ~1,000 | admin user setup (`admin_init`, `admin_login`, `admin_set_role`) |
| `steps/ten.rs` | ~1,000 | finalization (`finalize`, `cleanup`) |
| `steps/orchestrator.rs` | ~1,500 | `run_step`, `run_step_impl`, `StepRunner` |
| `steps/metadata.rs` | ~1,500 | `default_steps`, metadata helpers |
| `steps/errors.rs` | ~2,000 | `StepError`, `StepKind`, `StepStatus`, `StepOutcome`, `StepProgress` |
| `ops.rs` | unchanged | `Ops`, credential helpers (already <2,000) |
| `fake.rs` | unchanged | `FakeSystemOps`, `FakeCredentialStore` |
| `wizard.rs` | ~1,500 | `Wizard`, `WizardState`, `WizardError`, `WizardAction`, `WizardEvent` |
| `system.rs` | ~200 | `SystemOps`, `SystemOpsError`, `SystemOpsImpl` |
| tests | inline / `tests/` | keep tests as-is or move to `tests/` |

*Note:* `ops.rs` and `fake.rs` are already under 2,000 lines. Keep them as separate files;
`lib.rs` will re-export them.

### 3.2 Module naming and grouping

Group by **step number** and **domain**:
- Each step's `run_*` + helper functions go into `steps/{one, two, three, …, ten}.rs`.
- Error types and progress types live in `steps/errors.rs`.
- Orchestrator (`run_step`) lives in `steps/orchestrator.rs`.
- Step metadata (`default_steps`) lives in `steps/metadata.rs`.

## 4. Implementation steps

1. Create `crates/manager-wizard/src/steps/` directory.
2. Move step implementations into `steps/{one,two,three,four,five,six,seven,eight,nine,ten}.rs`.
3. Move error/progress types to `steps/errors.rs`.
4. Move orchestrator to `steps/orchestrator.rs`.
5. Move metadata to `steps/metadata.rs`.
6. Move `Ops`, `SystemOps` implementations to their own files if desired (already small).
7. Re-export from `lib.rs` (`pub use steps::one::*; pub use steps::two::*; ...`).
8. Move `fake.rs` contents to `fake.rs` (already there) and re-export.
9. Move `wizard` module contents to `wizard.rs` (or keep inline — either is fine).
10. Run `cargo check` and `cargo test -p manager-wizard`.

## 5. Definition of done

- [ ] `steps.rs` ≤ 2,000 lines.
- [ ] `lib.rs` ≤ 2,000 lines.
- [ ] `ops.rs` ≤ 2,000 lines (already true).
- [ ] Every new module compiles cleanly (`cargo check` on Windows).
- [ ] `cargo test -p manager-wizard` passes unchanged.
- [ ] `cargo test --workspace` passes unchanged.
- [ ] Public API unchanged: `cargo build` of `manager-app`, `manager-api`, `src-tauri` succeeds with no changes.
- [ ] No test renamed, deleted, or weakened.

## 6. Risks & pitfalls

- **Step function signatures.** Each step function (`step_one`, `step_two`, …) is called from
  `run_step_impl` via `match`. Do not change the function names or signatures.
- **`default_steps` order.** The `StepDefinition` array returned by `default_steps` must remain
  in the same order (`step_one` → `step_ten`) — changing order alters the install sequence.
- **`SystemOps` trait.** `SystemOps` is used by `ops.rs` (credential helpers). Keep the trait
  definition and its `credential_write` implementation unchanged.

# T-02c — Decompose `crates/manager-supervisor/src/lib.rs`

**Task ID:** T-02c (subtask of T-02)
**Category:** decomposition
**Blast radius:** `crates/manager-supervisor/src/`
**Est. effort:** L

## 1. Objective

Reduce `lib.rs` (4,027 lines) to **≤ 2,000 lines** by moving its function/type definitions into
cohesive submodules, preserving all signatures, names, and public API via `pub use` re-exports.

## 2. Current state (measured, clean checkout 2026-10-22)

| File | Lines | Key contents |
|------|-------|--------------|
| `crates/manager-supervisor/src/lib.rs` | 4,027 | `Supervisor`, `SupervisorState`, `SupervisorError`, `SupervisorAction`, `SupervisorEvent`, `SupervisorConfig`, `SupervisorResult`, `SupervisorHandle`, `SupervisorBuilder`, `SupervisorBuilderError`, tests |

Module map (from file scan):

| Section | Lines | Content |
|---------|-------|---------|
| `mod state` | inline | `SupervisorState`, `SupervisorError`, `SupervisorAction`, `SupervisorEvent` |
| `mod config` | inline | `SupervisorConfig`, `SupervisorResult`, `SupervisorHandle`, `SupervisorBuilder`, `SupervisorBuilderError` |
| `mod supervisor` | inline | `Supervisor`, `SupervisorImpl`, `SupervisorBuilder`, `SupervisorBuilderError` |
| tests | inline | tests |

## 3. Decomposition plan (concrete, module-by-module)

### 3.1 New modules

| Module | Target lines | Content | Re-export from `lib.rs` |
|--------|-------------|---------|------------------------|
| `state.rs` | ~1,000 | `SupervisorState`, `SupervisorError`, `SupervisorAction`, `SupervisorEvent` | `pub use state::*;` |
| `config.rs` | ~1,000 | `SupervisorConfig`, `SupervisorResult`, `SupervisorHandle`, `SupervisorBuilder`, `SupervisorBuilderError` | `pub use config::*;` |
| `supervisor.rs` | ~1,000 | `Supervisor`, `SupervisorImpl`, `SupervisorBuilder`, `SupervisorBuilderError` | `pub use supervisor::*;` |
| tests | inline / `tests/` | keep tests as-is or move to `tests/` |

## 4. Implementation steps

1. Create `crates/manager-supervisor/src/state.rs`, `config.rs`, `supervisor.rs`.
2. Move type definitions and impls into each file.
3. Re-export from `lib.rs` (`pub use state::*; pub use config::*; pub use supervisor::*;`).
4. Run `cargo check` and `cargo test -p manager-supervisor`.

## 5. Definition of done

- [ ] `lib.rs` ≤ 2,000 lines.
- [ ] Every new module compiles cleanly (`cargo check` on Windows).
- [ ] `cargo test -p manager-supervisor` passes unchanged.
- [ ] `cargo test --workspace` passes unchanged.
- [ ] Public API unchanged: `cargo build` of `manager-app`, `manager-api`, `src-tauri` succeeds with no changes.
- [ ] No test renamed, deleted, or weakened.

## 6. Risks & pitfalls

- **`Supervisor` is a large struct.** Do not change its fields or public methods — move only the
  `impl` blocks into `supervisor.rs`.
- **`SupervisorBuilder` is used by tests.** Keep its public API unchanged.

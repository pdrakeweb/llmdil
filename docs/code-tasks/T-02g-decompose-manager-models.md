# T-02g — Decompose `crates/manager-models/src/lib.rs`

**Task ID:** T-02g (subtask of T-02)
**Category:** decomposition
**Blast radius:** `crates/manager-models/src/`
**Est. effort:** M

## 1. Objective

Reduce `lib.rs` (2,303 lines) to **≤ 2,000 lines** by moving its function/type definitions into
cohesive submodules, preserving all signatures, names, and public API via `pub use` re-exports.

## 2. Current state (measured, clean checkout 2026-10-22)

| File | Lines | Key contents |
|------|-------|--------------|
| `crates/manager-models/src/lib.rs` | 2,303 | `Model`, `ModelError`, `ModelRegistry`, `ModelSpec`, `ModelStatus`, tests |

Module map (from file scan):

| Section | Lines | Content |
|---------|-------|---------|
| `mod types` | inline | `Model`, `ModelError`, `ModelSpec`, `ModelStatus` |
| `mod registry` | inline | `ModelRegistry`, `ModelRegistryError`, `ModelRegistryResult` |
| tests | inline | tests |

## 3. Decomposition plan (concrete, module-by-module)

### 3.1 New modules

| Module | Target lines | Content | Re-export from `lib.rs` |
|--------|-------------|---------|------------------------|
| `types.rs` | ~1,000 | `Model`, `ModelError`, `ModelSpec`, `ModelStatus` | `pub use types::*;` |
| `registry.rs` | ~1,000 | `ModelRegistry`, `ModelRegistryError`, `ModelRegistryResult` | `pub use registry::*;` |
| tests | inline / `tests/` | keep tests as-is or move to `tests/` |

## 4. Implementation steps

1. Create `crates/manager-models/src/types.rs`, `registry.rs`.
2. Move type definitions and impls into each file.
3. Re-export from `lib.rs` (`pub use types::*; pub use registry::*;`).
4. Run `cargo check` and `cargo test -p manager-models`.

## 5. Definition of done

- [ ] `lib.rs` ≤ 2,000 lines.
- [ ] Every new module compiles cleanly (`cargo check` on Windows).
- [ ] `cargo test -p manager-models` passes unchanged.
- [ ] `cargo test --workspace` passes unchanged.
- [ ] Public API unchanged: `cargo build` of `manager-app`, `manager-api`, `src-tauri` succeeds with no changes.
- [ ] No test renamed, deleted, or weakened.

## 6. Risks & pitfalls

- **`ModelRegistry` is a large struct.** Do not change its fields or public methods — move only the
  `impl` blocks into `registry.rs`.
- **`ModelSpec` is used widely.** Do not change its fields or public API.

# T-02e — Decompose `crates/manager-gateway/src/lib.rs`

**Task ID:** T-02e (subtask of T-02)
**Category:** decomposition
**Blast radius:** `crates/manager-gateway/src/`
**Est. effort:** M

## 1. Objective

Reduce `lib.rs` (2,996 lines) to **≤ 2,000 lines** by moving its function/type definitions into
cohesive submodules, preserving all signatures, names, and public API via `pub use` re-exports.

## 2. Current state (measured, clean checkout 2026-10-22)

| File | Lines | Key contents |
|------|-------|--------------|
| `crates/manager-gateway/src/lib.rs` | 2,996 | `Gateway`, `GatewayError`, `GatewayConfig`, `GatewayHandle`, `GatewayBuilder`, `GatewayBuilderError`, tests |

Module map (from file scan):

| Section | Lines | Content |
|---------|-------|---------|
| `mod state` | inline | `GatewayState`, `GatewayError`, `GatewayAction`, `GatewayEvent` |
| `mod config` | inline | `GatewayConfig`, `GatewayResult`, `GatewayHandle`, `GatewayBuilder`, `GatewayBuilderError` |
| `mod gateway` | inline | `Gateway`, `GatewayImpl`, `GatewayBuilder`, `GatewayBuilderError` |
| tests | inline | tests |

## 3. Decomposition plan (concrete, module-by-module)

### 3.1 New modules

| Module | Target lines | Content | Re-export from `lib.rs` |
|--------|-------------|---------|------------------------|
| `state.rs` | ~1,000 | `GatewayState`, `GatewayError`, `GatewayAction`, `GatewayEvent` | `pub use state::*;` |
| `config.rs` | ~1,000 | `GatewayConfig`, `GatewayResult`, `GatewayHandle`, `GatewayBuilder`, `GatewayBuilderError` | `pub use config::*;` |
| `gateway.rs` | ~1,000 | `Gateway`, `GatewayImpl`, `GatewayBuilder`, `GatewayBuilderError` | `pub use gateway::*;` |
| tests | inline / `tests/` | keep tests as-is or move to `tests/` |

## 4. Implementation steps

1. Create `crates/manager-gateway/src/state.rs`, `config.rs`, `gateway.rs`.
2. Move type definitions and impls into each file.
3. Re-export from `lib.rs` (`pub use state::*; pub use config::*; pub use gateway::*;`).
4. Run `cargo check` and `cargo test -p manager-gateway`.

## 5. Definition of done

- [ ] `lib.rs` ≤ 2,000 lines.
- [ ] Every new module compiles cleanly (`cargo check` on Windows).
- [ ] `cargo test -p manager-gateway` passes unchanged.
- [ ] `cargo test --workspace` passes unchanged.
- [ ] Public API unchanged: `cargo build` of `manager-app`, `manager-api`, `src-tauri` succeeds with no changes.
- [ ] No test renamed, deleted, or weakened.

## 6. Risks & pitfalls

- **`Gateway` is a large struct.** Do not change its fields or public methods — move only the
  `impl` blocks into `gateway.rs`.
- **`GatewayBuilder` is used by tests.** Keep its public API unchanged.

# T-02d — Decompose `crates/manager-config/src/lib.rs`

**Task ID:** T-02d (subtask of T-02)
**Category:** decomposition
**Blast radius:** `crates/manager-config/src/`
**Est. effort:** L

## 1. Objective

Reduce `lib.rs` (3,795 lines) to **≤ 2,000 lines** by moving its function/type definitions into
cohesive submodules, preserving all signatures, names, and public API via `pub use` re-exports.

## 2. Current state (measured, clean checkout 2026-10-22)

| File | Lines | Key contents |
|------|-------|--------------|
| `crates/manager-config/src/lib.rs` | 3,795 | `Config`, `ConfigError`, `ConfigKey`, `ConfigManager`, `ConfigValue`, `ConfigChange`, `ConfigEvent`, tests |

Module map (from file scan):

| Section | Lines | Content |
|---------|-------|---------|
| `mod types` | inline | `Config`, `ConfigError`, `ConfigKey`, `ConfigManager`, `ConfigValue`, `ConfigChange`, `ConfigEvent` |
| `mod utils` | inline | `config_path`, `load_config`, `save_config`, `validate_config` |
| tests | inline | tests |

## 3. Decomposition plan (concrete, module-by-module)

### 3.1 New modules

| Module | Target lines | Content | Re-export from `lib.rs` |
|--------|-------------|---------|------------------------|
| `types.rs` | ~2,000 | `Config`, `ConfigError`, `ConfigKey`, `ConfigManager`, `ConfigValue`, `ConfigChange`, `ConfigEvent` | `pub use types::*;` |
| `utils.rs` | ~1,000 | `config_path`, `load_config`, `save_config`, `validate_config` | `pub use utils::*;` |
| tests | inline / `tests/` | keep tests as-is or move to `tests/` |

## 4. Implementation steps

1. Create `crates/manager-config/src/types.rs`, `utils.rs`.
2. Move type definitions and impls into each file.
3. Re-export from `lib.rs` (`pub use types::*; pub use utils::*;`).
4. Run `cargo check` and `cargo test -p manager-config`.

## 5. Definition of done

- [ ] `lib.rs` ≤ 2,000 lines.
- [ ] Every new module compiles cleanly (`cargo check` on Windows).
- [ ] `cargo test -p manager-config` passes unchanged.
- [ ] `cargo test --workspace` passes unchanged.
- [ ] Public API unchanged: `cargo build` of `manager-app`, `manager-api`, `src-tauri` succeeds with no changes.
- [ ] No test renamed, deleted, or weakened.

## 6. Risks & pitfalls

- **`ConfigKey` is an enum used widely.** Do not change its variants or public API.
- **`ConfigManager` is used by `AppState`.** Keep its public methods unchanged.

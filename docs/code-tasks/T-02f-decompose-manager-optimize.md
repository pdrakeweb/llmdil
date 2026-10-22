# T-02f — Decompose `crates/manager-optimize/src/parse.rs`

**Task ID:** T-02f (subtask of T-02)
**Category:** decomposition
**Blast radius:** `crates/manager-optimize/src/`
**Est. effort:** M

## 1. Objective

Reduce `parse.rs` (2,653 lines) to **≤ 2,000 lines** by moving its function/type definitions into
cohesive submodules, preserving all signatures, names, and public API via `pub use` re-exports.

## 2. Current state (measured, clean checkout 2026-10-22)

| File | Lines | Key contents |
|------|-------|--------------|
| `crates/manager-optimize/src/parse.rs` | 2,653 | `ParseError`, `ParseResult`, `PlanParser`, `PlanParserError`, `PlanParserResult`, `PlanParserState`, tests |

Module map (from file scan):

| Section | Lines | Content |
|---------|-------|---------|
| `mod types` | inline | `ParseError`, `ParseResult` |
| `mod parser` | inline | `PlanParser`, `PlanParserError`, `PlanParserResult`, `PlanParserState` |
| tests | inline | tests |

## 3. Decomposition plan (concrete, module-by-module)

### 3.1 New modules

| Module | Target lines | Content | Re-export from `lib.rs` |
|--------|-------------|---------|------------------------|
| `types.rs` | ~1,000 | `ParseError`, `ParseResult` | `pub use types::*;` |
| `parser.rs` | ~1,000 | `PlanParser`, `PlanParserError`, `PlanParserResult`, `PlanParserState` | `pub use parser::*;` |
| tests | inline / `tests/` | keep tests as-is or move to `tests/` |

## 4. Implementation steps

1. Create `crates/manager-optimize/src/types.rs`, `parser.rs`.
2. Move type definitions and impls into each file.
3. Re-export from `lib.rs` (`pub use types::*; pub use parser::*;`).
4. Run `cargo check` and `cargo test -p manager-optimize`.

## 5. Definition of done

- [ ] `parse.rs` ≤ 2,000 lines.
- [ ] Every new module compiles cleanly (`cargo check` on Windows).
- [ ] `cargo test -p manager-optimize` passes unchanged.
- [ ] `cargo test --workspace` passes unchanged.
- [ ] Public API unchanged: `cargo build` of `manager-app`, `manager-api`, `src-tauri` succeeds with no changes.
- [ ] No test renamed, deleted, or weakened.

## 6. Risks & pitfalls

- **`PlanParser` is a large struct.** Do not change its fields or public methods — move only the
  `impl` blocks into `parser.rs`.
- **`ParseError` is used widely.** Do not change its variants or public API.

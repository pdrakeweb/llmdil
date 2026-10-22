# T-02 — Decompose god files (umbrella + shared rules)

**Task ID:** T-02 (umbrella) → per-crate subtasks **T-02a … T-02g**
**Category:** decomposition
**Review source:** `LLMdil-systems-review.md` §3.2, §5.2
**Est. effort:** umbrella (subtasks are independently dispatchable)
**Depends on:** none (each subtask is independent; see ordering note in `README.md`)

## 1. Objective

Break the oversized source files into cohesive modules so that **no source file exceeds 2,000
lines** and each file has a single, nameable responsibility. This is a **pure reorganization**:
code is *moved* into new module files; signatures, names, behavior, and public API are unchanged.

The per-crate work is split into **independently-implementable subtasks** so each can be
dispatched to a separate coding agent:

| Subtask | File | Target file(s) | Lines |
|---------|------|----------------|-------|
| **T-02a** | `T-02a-decompose-manager-app.md` | `crates/manager-app/src/lib.rs`, `crates/manager-app/src/optimize.rs` | 15,077 + 3,322 |
| **T-02b** | `T-02b-decompose-manager-wizard.md` | `crates/manager-wizard/src/steps.rs`, `lib.rs`, `ops.rs` | 12,596 + 3,113 + 2,017 |
| **T-02c** | `T-02c-decompose-manager-supervisor.md` | `crates/manager-supervisor/src/lib.rs` | 4,027 |
| **T-02d** | `T-02d-decompose-manager-config.md` | `crates/manager-config/src/lib.rs` | 3,795 |
| **T-02e** | `T-02e-decompose-manager-gateway.md` | `crates/manager-gateway/src/lib.rs` | 2,996 |
| **T-02f** | `T-02f-decompose-manager-optimize.md` | `crates/manager-optimize/src/parse.rs` | 2,653 |
| **T-02g** | `T-02g-decompose-manager-models.md` | `crates/manager-models/src/lib.rs` | 2,303 |
| **T-02h** | `T-02h-decompose-ui.md` | `ui/src/views/static.ts`, `ui/src/api.ts` | 8,816 + 3,868 |

## 2. Context / why

Five files dominate the codebase and each bundles many unrelated responsibilities:
- `manager-app/src/lib.rs` (15,077 lines) holds the `AppState` struct, ~32 lock fields, and the
  bodies of all 130 `op_*` functions plus several inline modules — it is both the state pile and
  the operation implementation pile.
- `manager-wizard/src/steps.rs` (12,596 lines) holds all 10 install-step implementations.
- `ui/src/views/static.ts` (8,816 lines) and `ui/src/api.ts` (3,868 lines) bundle the whole
  frontend view logic and the hand-written API client.

Large single files make every change high-risk (wide blast radius), slow to review, and hard to
test in isolation. Splitting by responsibility shrinks the blast radius of future changes and
enables independent, verifiable refactors.

## 3. Shared decomposition rules (bind on EVERY subtask)

These rules are mandatory for every T-02x subtask:

1. **Move bodies only.** Extract functions/types into new module files; do **not** change function
   signatures, type definitions, field names, serialization names, route strings, or op names.
2. **No behavioral change.** The observable behavior and the full test suite must remain
   semantically identical. No test may be renamed (except the single allowed registry-test rename
   noted in T-01), deleted, or weakened. New tests may be added.
3. **Visibility is the only mechanism that may change.** To move an item into a submodule you may
   need to widen `pub`/`pub(crate)` visibility or add `pub use` re-exports. **Preserve the crate's
   public API exactly** by re-exporting from the original path (e.g. `pub use ops::tasks::*;` in
   `lib.rs`) so downstream crates (`manager-api`, `manager-mcp`, `src-tauri`) need no changes.
4. **Keep the operation registry intact.** `OP_SPECS.len() == 130`; all op names/params/routes/
   classes unchanged. The `for_each_operation!` declaration block stays in `registry.rs`.
5. **`AppState` moves as a whole in this task family only if a subtask says so.** Do not *restructure*
   `AppState`'s fields here (that is T-03). You may *move* the `AppState` definition into a new
   module file, but its fields and types stay identical.
6. **Target: no file > 2,000 lines** after the change. Aim for cohesive modules of a few hundred
   to ~1,500 lines.
7. **Verify on Windows.** `#[cfg(windows)]` code is not compiled on Linux CI; the subtask must be
   verified with a Windows `cargo check`/`cargo build` and the full test suite.
8. **Each subtask lists its own blast radius** — touch only that crate/module (and the re-export
   shims in its `lib.rs`). Do not reach into other crates.

## 4. Verification template (applies to every subtask)

For each subtask, success = all of:
- `cargo check` (Windows) clean; no new `clippy -D warnings` violations.
- `cargo test -p <crate>` passes; `cargo test --workspace` passes (same feature flags as CI).
- `OP_SPECS.len() == 130` (for any subtask that touches `manager-app`).
- The target file(s) are now ≤ 2,000 lines; each new module file is cohesive (one responsibility).
- Public API unchanged: `cargo build` of downstream crates (`manager-api`, `manager-mcp`,
  `src-tauri`) succeeds with **no** source changes in those crates.
- The subtask's own *Definition of done* checklist is green.

## 5. Cross-subtask notes

- **T-02a and T-03 both edit `manager-app/src/lib.rs`.** Do one, land it, then the other (or merge).
  T-02a moves code out; T-03 restructures `AppState`. They are independently verifiable either way.
- **T-02a is the largest (15,077 lines).** It naturally splits into: (a) the `op_*` function bodies
  → per-domain `ops/` modules, (b) the `AppState` definition + `impl` → a `state.rs` module
  (definitions only — T-03 later restructures it), (c) the inline payload/`events` modules are
  already separate. See T-02a for the exact plan.
- **Subtasks T-02b…T-02h are independent of each other** and can run in parallel.
- **Every Rust file > 2,000 lines is covered** by exactly one subtask (verified by a full
  repo scan on 2026-10-22): `manager-app/{lib,optimize}.rs`, `manager-wizard/{steps,lib,ops}.rs`,
  `manager-supervisor/lib.rs`, `manager-config/lib.rs`, `manager-gateway/lib.rs`,
  `manager-optimize/parse.rs`, `manager-models/lib.rs`, plus `ui/{views/static,api}.ts`.

## 6. What this umbrella task does NOT do

- Does not restructure `AppState` (T-03), does not add typed envelopes (T-01), does not add CI
  (T-04), logging (T-05), or platform seams (T-06).
- Does not touch files under 2,000 lines (e.g. `manager-winml`, `manager-telemetry` modules,
  `manager-services`).

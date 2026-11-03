# T-01 — Extend registry codegen to emit typed request/response types, generated TS types, and per-op JSON Schema

**Task ID:** T-01
**Category:** structural
**Review source:** `LLMdil-systems-review.md` §3.3, §5.1
**Blast radius:** `crates/manager-app/src/registry.rs`, `crates/manager-app/src/` (new generated module), `src-tauri/src/main.rs`, `ui/` (new generated TS file), `crates/manager-app/Cargo.toml` (build script only if needed)
**Est. effort:** L
**Depends on:** none (foundational)

## 1. Objective

Extend the existing `for_each_operation!` registry so that, from the **single declaration block**
that already produces the `Op` enum, `OP_SPECS`, the dispatch match, and the 130 Tauri command
adapters, it **also** generates:

1. **Typed request structs** in Rust — one `#[derive(Deserialize, Clone, Debug)]` struct per op,
   with one field per declared parameter (field name = the parameter name already in the
   declaration), replacing the current untyped `serde_json::Value` + manual `arg::<T>` extraction
   at the boundary.
2. **Generated TypeScript types** — a committed (or build-emitted) `ui/src/generated/ops.ts`
   exposing a typed request interface per op, so the UI stops hand-writing its own divergent types.
3. **Per-op JSON Schema** — a machine-readable schema per op (parameter names, types, required
   flags) emitted to a JSON file (e.g. `ui/src/generated/ops.schema.json` or a Rust test artifact),
   usable by tooling, the future MCP server, and validation.

This does **not** change any op's name, parameter names, types, route, capability class, or
behavior. It only tightens the *boundary* from untyped JSON to typed values.

## 2. Context / why

All 130 ops today take/return `serde_json::Value` and pull parameters out with a manual
`arg::<T>(&args, "name")` call inside `dispatch` (see `gen_dispatch!` in `registry.rs`). The UI
then hand-writes its own request types in `ui/src/api.ts` (~3,868 lines). Result: **silent drift**
between what the UI thinks an op accepts and what the registry actually declares. The registry
already has a drift test (`frontend_invocations_are_all_registered`) that only checks *names*
are registered — not that *shapes* match. Typed envelopes + generated TS + JSON Schema close that
gap mechanically: one declaration, three generated artifacts, all provably in sync.

## 3. Current state (measured, clean checkout 2026-10-22)

File: `crates/manager-app/src/registry.rs` (**644 lines** total).

- **`CapClass`** enum (line ~30): `R, SConfig, SDisclosure, L, C, D, X, UserAdmin, ApiSettings`
  with `pub const ALL`. Capability classes drive `role_allows` and the admin-API `OpSpec.class`.
- **`Role`** enum + **`role_allows(role, cap)`** (lines ~122–138): the authorization matrix.
- **`OpSpec`** struct (line **158**):
  ```rust
  pub struct OpSpec {
      pub op: Op,
      pub name: &'static str,
      pub class: CapClass,
      pub params: &'static [(&'static str, Option<&'static str>)], // (name, type) — type is a doc hint
      pub route: Option<Route>,
  }
  ```
- **`Route`** enum (line ~148): `Get(#[non_exhaustive] &'static str)`, `Post(#[non_exhaustive] &'static str)`.
- **`macro_rules! for_each_operation`** (line **184**). Each arm is a tuple:
  ```
  (Variant, "op_name", Class, op_fn_name, [pname: pty, pname2: pty2, ...], route_expr)
  ```
  where `route_expr` is `None` or `Some(route(...))` / `Some(route_wrap(...))`.
- **Declaration block** (lines ~185–387): the 130 arms. Example arms:
  - line 232: `(GetTasks, "get_tasks", R, op_get_tasks, [], None)`
  - line 245: `(StartTask, "start_task", C, op_start_task, [task_id: String], Some(route("/api/v1/tasks/:task_id/start")))`
- **`gen_op_enum!`** (lines ~391–404) → `pub enum Op`.
- **`gen_op_specs!`** (lines ~406–421) → `pub static OP_SPECS: &[OpSpec]`.
- **`gen_dispatch!`** (lines ~423–442) → `pub async fn dispatch(state, sink, op, args)`. Each arm
  currently does `crate::$op_fn(state, sink, arg::<$pty>(&args, stringify!($pname))?)`.
- **`pub fn spec(op)`** (line 444), **`pub fn op_by_name(name)`** (line 449).
- **Drift/contract tests** (lines ~448–644):
  - `registry_covers_all_82_commands` (line **450**) — *note: the name is stale; it asserts `OP_SPECS.len() == 130`* (line 451).
  - `names_and_routes_are_unique`, `every_spec_round_trips_through_lookup`,
    `role_matrix_matches_plan`, `frontend_invocations_are_all_registered` (scans `ui/src/api.ts`,
    has a `SHELL_LOCAL` allow-list of UI-only calls), `dispatch_validates_arguments_before_running`,
    `dispatch_reaches_the_operation`.

File: `src-tauri/src/main.rs` (**703 lines**):
- **`macro_rules! tauri_adapters`** (lines ~76–99) generates one `#[tauri::command]` per op,
  reading `args` and calling the shared `invoke_op` path.
- `manager_app::for_each_operation!(tauri_adapters)` (line **99**).
- `tauri::generate_handler![...]` (lines ~107–127): the generated adapters plus a hand-written
  `admin_api_*` list.
- `main()` (line 150) wires the app; background loops at lines ~214–266.

The `arg!`/`arg_u8!`/`arg::<T>` extraction helpers live in `manager-app/src/` (search
`macro_rules! arg` / `fn arg<`); they are used inside `dispatch` and inside individual `op_*`
functions when they need to read nested fields.

## 4. Scope

### In scope
- Add codegen (in the same macro or a companion macro/build step) that emits:
  - One Rust request struct per op (e.g. `GetTasksReq`, `StartTaskReq`, …) with
    `#[serde(rename_all = "snake_case")]` fields matching the declared parameter names, and
    `Option<T>` for parameters that are currently optional (a parameter is optional if every
    current `op_*`/`arg` use treats it as `Option` or defaults it — inspect each op; when in
    doubt, keep it required and have the op read the same value as today).
  - A generated `ui/src/generated/ops.ts` with a `readonly` interface per op request.
  - A generated per-op JSON Schema (parameters: name, type, required).
- Plumb the typed request struct through `dispatch` **without changing the wire format**:
  `dispatch` should still accept the same `args` it accepts today and deserialize into the
  request struct, so all existing callers (admin API, Tauri adapters) keep working unchanged.
- Add tests that assert the generated artifacts are in sync with `OP_SPECS`.

### Out of scope (do NOT do)
- Do **not** rename, add, remove, or reorder any op, parameter, route, or capability class.
- Do **not** change `OpSpec`, the `Op` enum variants, `OP_SPECS` contents, `dispatch`'s public
  signature, or any `op_*` function's public signature.
- Do **not** change the wire format of any admin-API endpoint or Tauri command.
- Do **not** rewrite the UI's hand-written types to use the generated ones *in this task* (that is
  a follow-on); just *emit* the generated TS file and keep it compiling.
- Do **not** change `OP_SPECS.len()` (stays 130).
- Do **not** touch `AppState` or the decomposition of `lib.rs`.

## 5. Invariants that must hold (no behavior change)

- `OP_SPECS.len() == 130`; every existing op name, parameter name, parameter type, route, and
  `CapClass` unchanged.
- `dispatch` still accepts the same `args` and still returns the same results for the same input.
- All admin-API routes and Tauri command names unchanged.
- The six registry drift tests in §3 keep passing **without modification** (the stale test name
  `registry_covers_all_82_commands` may be renamed to `registry_covers_all_130_commands` — that is
  the *only* allowed rename, and the assertion stays `== 130`).
- `frontend_invocations_are_all_registered` keeps passing (the UI still calls the same op names).

## 6. Implementation plan

1. **Decide the emission mechanism.** Two acceptable options — pick one and document it:
   - **(A) Macro-level:** extend `for_each_operation!` (or add sibling macros `gen_req_structs!`,
     `gen_ts!`, `gen_json_schema!`) so the single declaration block drives all artifacts. Preferred
     for keeping a single source of truth.
   - **(B) Build script / test-time emitter:** a `#[test]` (or `build.rs`) that reads `OP_SPECS`
     and writes `ui/src/generated/ops.ts` + `ops.schema.json`. Simpler to start, but the Rust
     request structs still need macro/codegen.
2. **Generate the Rust request structs.** For each op, emit
   `pub struct <Variant>Req { pub <pname>: <T> or Option<<T>> }` with
   `#[derive(Deserialize, Clone, Debug, Default)]`. Map the `pty` hints (`String`, `u8`, `bool`,
   `u32`, etc.) to concrete Rust types. Keep a `From<serde_json::Value>` or a
   `serde_json::from_value::<Req>(args)` path so `dispatch` can deserialize without changing
   callers.
3. **Wire `dispatch` to deserialize.** Inside `gen_dispatch!`, replace the per-field
   `arg::<$pty>(&args, ...)` calls with a single
   `let req: $VariantReq = serde_json::from_value(args.clone()).map_err(AppError::BadRequest)?;`
   and pass `req.<pname>` to the op. Keep the op's signature if it currently takes the raw value —
   i.e. change *how the value is obtained*, not *what the op does*.
4. **Generate `ui/src/generated/ops.ts`.** Emit one interface per op with snake_case→camelCase
   fields matching the Rust struct, plus an `OpName` union type. Commit it (or emit on build and
   git-ignore it — pick one; if committed, add a test that it is current).
5. **Generate per-op JSON Schema.** Emit `ops.schema.json` (a map op name → `{type, properties,
   required}`).
6. **Add sync tests:**
   - `request_structs_match_op_specs` — for every op in `OP_SPECS`, deserialize a sample
     `serde_json::Value` into the request struct and confirm field names match `spec.params`.
   - `ts_types_match_op_specs` — parse `ui/src/generated/ops.ts` (string check) for an interface
     per op and the same field names.
   - `json_schema_matches_op_specs` — for every op, the schema's `properties` keys equal
     `spec.params` names and `required` flags match.
7. **Update the stale test name** `registry_covers_all_82_commands` →
   `registry_covers_all_130_commands` (assertion unchanged, stays `== 130`).

## 7. Testing & verification

Run (Windows toolchain):
```
cargo test -p manager-app
cargo test --workspace            # same feature flags as CI
```
Success criteria:
- Compiles on Windows; no new warnings that break `clippy -D warnings` (keep it clean).
- All pre-existing registry tests pass unchanged (plus the one allowed rename).
- New sync tests (request structs / TS / JSON Schema) pass and would fail if a declaration drifted.
- `OP_SPECS.len()` is still 130.
- Spot-check: `dispatch` for `GetTasks` (no params) and `StartTask` (`task_id: String`) returns
  identical results to before for the same `args`.

## 8. Definition of done / acceptance criteria

- [ ] One typed request struct per op, generated from the single declaration block.
- [ ] `ui/src/generated/ops.ts` exists, compiles, and has one interface per op with matching field names.
- [ ] Per-op JSON Schema emitted (committed or build-emitted) and validated against `OP_SPECS`.
- [ ] `dispatch` deserializes into the request struct; wire format and op behavior unchanged.
- [ ] `OP_SPECS.len() == 130`; no op/param/route/class renamed.
- [ ] All registry drift tests pass (one allowed rename: `…_82_commands` → `…_130_commands`).
- [ ] New sync tests added and passing.
- [ ] No test deleted or weakened; no behavioral change.

## 9. Risks & pitfalls

- **Optional vs required parameters.** Some ops treat a parameter as optional (defaulted or
  `Option`). Mis-classifying it changes deserialization. Inspect each op's current `arg` usage;
  when uncertain, keep the field required (matches today's `arg` which errors on missing) — do not
  silently add defaults.
- **`serde` renames.** Field names in the request struct must match the exact JSON parameter names
  the UI/API send today (snake_case). Do not `rename_all` in a way that diverges from current keys.
- **The stale test name.** Renaming `registry_covers_all_82_commands` is allowed; do not change its
  `== 130` assertion, and do not rename any *other* test.
- **Single source of truth.** If you choose option (B) (build-time emitter from `OP_SPECS`), the
  Rust request structs still need macro codegen — don't hand-write 130 structs, that reintroduces
  the exact drift this task removes.
- **Keep it a boundary change.** The op bodies themselves are out of scope; if an op reads a nested
  field from `args` beyond the declared top-level params, leave that read as-is (the request struct
  only covers declared params).

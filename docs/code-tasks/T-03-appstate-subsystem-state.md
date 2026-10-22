# T-03 — Replace the `AppState` ad-hoc lock pile with per-subsystem state objects

**Task ID:** T-03
**Category:** structural
**Review source:** `LLMdil-systems-review.md` §3.1, §5.3
**Blast radius:** `crates/manager-app/src/lib.rs`, `crates/manager-app/src/state/`, possibly `manager-wizard/src/ops.rs`
**Est. effort:** XL
**Depends on:** T-02a (decompose `manager-app/src/lib.rs`) — do T-02a first or merge

## 1. Objective

Replace the 32 ad-hoc `OnceCell`/`Mutex`/`RwLock` fields in `AppState` with **per-subsystem
state objects** (e.g. `DownloadManagerState`, `TaskManagerState`, `UserManagerState`,
`TelemetryState`, `ModelsState`, `LogsState`, `MetricsState`, `AdminState`, `WizardState`).
Each subsystem owns its own locks and data, reducing contention and making `AppState` a thin
container for subsystem handles.

## 2. Context / why

Today `AppState` holds 32 individual locks (`OnceCell`, `Mutex`, `RwLock`) — a **lock pile**
with documented invariants only in comments. This makes reasoning about concurrency hard,
increases contention, and obscures subsystem boundaries.

By moving each subsystem's data + locks into its own state object (e.g. `DownloadManagerState`),
we:
- Encapsulate subsystem state and locking policy.
- Reduce `AppState` to ~10–15 fields (subsystem handles, not raw locks).
- Make future decomposition easier (subsystems can evolve independently).
- Improve testability (subsystem state can be mocked independently).

## 3. Current state (measured, clean checkout 2026-10-22)

`AppState` (line ~158 in `lib.rs`) has **32 fields** (grouped by subsystem):

| Subsystem | Fields | Types |
|-----------|--------|-------|
| config | `config`, `config_path`, `config_write_lock`, `config_read_lock` | `RwLock`, `OnceCell`, `Mutex` |
| downloads | `downloads`, `downloads_path` | `Mutex`, `OnceCell` |
| tasks | `tasks`, `tasks_path` | `Mutex`, `OnceCell` |
| users | `users`, `users_path` | `Mutex`, `OnceCell` |
| telemetry | `telemetry`, `telemetry_enabled`, `telemetry_path` | `RwLock`, `OnceCell` |
| models | `models`, `models_path` | `RwLock`, `OnceCell` |
| gpu | `gpu` | `Mutex` |
| supervisor | `supervisor`, `supervisor_path` | `Mutex`, `OnceCell` |
| cache | `cache`, `cache_path` | `RwLock`, `OnceCell` |
| venv | `venv`, `venv_path` | `OnceCell`, `OnceCell` |
| admin | `admin`, `admin_path` | `OnceCell`, `OnceCell` |
| metrics | `metrics`, `metrics_path` | `RwLock`, `OnceCell` |
| logs | `logs`, `logs_path` | `Mutex`, `OnceCell` |
| events | `events` | `Mutex` |
| wizard | `wizard` | `Mutex` |

## 4. Scope

### In scope
- Define a **per-subsystem state interface** (e.g. `trait DownloadManagerState { fn get(&self) -> … }`)
  for each subsystem.
- Create a **concrete state object** per subsystem (e.g. `struct DownloadManagerState {
  manager: Mutex<DownloadManager>, path: OnceCell<PathBuf>,`…`}`).
- Replace `AppState`'s raw fields with **handles to subsystem state objects**.
- Update `AppState`'s `impl` blocks to use the new handles.
- Update `config_write_lock`/`config_read_lock` to be internal to `ConfigState`.
- Keep the **public API of `AppState` unchanged** (all `pub fn` stay public, signatures unchanged).

### Out of scope (do NOT do)
- Do **not** change `AppState`'s public methods or signatures — keep them identical.
- Do **not** change `AppState`'s field names — keep them for backwards compatibility with
  downstream crates (`src-tauri`, `manager-api`).
- Do **not** change the `OnceCell`/`Mutex`/`RwLock` wrappers themselves — just move them
  into the subsystem state objects.
- Do **not** change `AppState`'s initialization logic (`get_app_state`, `set_app_state`).

## 5. Invariants that must hold

- `AppState` remains a `pub struct` with the **same 32 field names** (`config`, `config_path`,
  `downloads`, …) — no rename, add, or remove.
- `AppState`'s **public methods** (`config_write_lock`, `config_read_lock`, `get_app_state`,
  `set_app_state`, etc.) keep their signatures and behavior.
- Every `pub use AppState` in `lib.rs` must still resolve to the same struct (no new type alias).
- All downstream crates (`src-tauri`, `manager-api`) that reference `AppState` compile unchanged.

## 6. Implementation plan

1. **Create `crates/manager-app/src/state/` directory** with one file per subsystem:
   - `config.rs`, `downloads.rs`, `tasks.rs`, `users.rs`, `telemetry.rs`, `models.rs`,
     `gpu.rs`, `supervisor.rs`, `cache.rs`, `venv.rs`, `admin.rs`, `metrics.rs`,
     `logs.rs`, `events.rs`, `wizard.rs`.
2. **Define state interfaces** (optional but recommended) — e.g.:
   ```rust
   pub trait ConfigState {
       fn config(&self) -> RwLockReadGuard<AppConfig>;
       fn config_write_lock(&self) -> &Mutex<()>;
       fn config_read_lock(&self) -> &Mutex<()>;
   }
   ```
3. **Define concrete state objects** — e.g.:
   ```rust
   pub struct ConfigState {
       config: RwLock<AppConfig>,
       path: OnceCell<PathBuf>,
       write_lock: Mutex<()>,
       read_lock: Mutex<()>,
   }
   impl ConfigState {
       pub fn new() -> Self { /* ... */ }
       pub fn config(&self) -> RwLockReadGuard<AppConfig> { self.config.read() }
       // ...
   }
   ```
4. **Replace `AppState` fields** — e.g.:
   ```rust
   pub struct AppState {
       // old: config: RwLock<AppConfig>, config_path: OnceCell<PathBuf>, ...
       config: ConfigState,
       // old: downloads: Mutex<DownloadManager>, downloads_path: OnceCell<PathBuf>, ...
       downloads: DownloadManagerState,
       // ...
   }
   ```
5. **Update `AppState`'s `impl` blocks** — replace raw field access with method calls.
6. **Run `cargo check`** (Windows) and `cargo test --workspace`.
7. **Verify downstream crates** (`cargo build -p src-tauri`, `cargo build -p manager-api`) still compile.

## 7. Testing & verification

- `cargo check` (Windows) — no new warnings.
- `cargo test -p manager-app` passes unchanged.
- `cargo test --workspace` passes unchanged.
- `cargo build -p src-tauri` and `cargo build -p manager-api` succeed with no source changes.

## 8. Definition of done

- [ ] `AppState` has the same 32 field names and public method signatures as before.
- [ ] Every subsystem's state is encapsulated in its own state object.
- [ ] `AppState`'s `impl` blocks use the subsystem state objects.
- [ ] `cargo check` (Windows), `cargo test -p manager-app`, and `cargo test --workspace` pass.
- [ ] Downstream crates (`src-tauri`, `manager-api`) compile unchanged.
- [ ] No test renamed, deleted, or weakened.

## 9. Risks & pitfalls

- **Field name stability.** Do not rename any `AppState` field — downstream crates (including
  `src-tauri`) reference them by name. If you must rename, keep an old-named field that aliases
  to the new one and deprecate it — but that defeats the purpose. Better: leave the field names
  identical and store the subsystem state in the same slot.
- **`config_write_lock`/`config_read_lock`.** These are called from `lib.rs`. Keep them as
  `pub fn` on `AppState`, even if they delegate to `self.config.write_lock()` internally.
- **`AppState` is a global.** The `get_app_state`/`set_app_state` pattern means `AppState` lives
  in a `OnceCell`. Do not change that pattern — just wrap the inner data in subsystem state objects.

# T-05 — One logging pipeline: init `tracing`, `task_id` spans, feed diagnostics export

**Task ID:** T-05
**Category:** structural
**Review source:** `LLMdil-systems-review.md` §3.6, §5.5
**Blast radius:** `src-tauri/src/main.rs`, `crates/manager-app/src/lib.rs`, `crates/manager-app/src/registry.rs`, `crates/manager-telemetry/src/`
**Est. effort:** M

## 1. Objective

Implement a **single logging pipeline**:
- Initialize `tracing-subscriber` in `src-tauri/src/main.rs`.
- Attach a `task_id` field to every `tracing::span!` in the app.
- Feed logs into the diagnostics export (JSON Lines to `logs/` directory or SSE stream).
- Add `tracing::` call sites in `manager-app` where `eprintln!` currently appears.

## 2. Context / why

Today the codebase has:
- ~20 `tracing::` sites (mostly in `manager-telemetry`).
- **Zero** `tracing_subscriber` initialization (events silently discarded).
- ~32 `eprintln!` calls (fallback logging).
- No `task_id` attached to spans.

This means:
- Logs are lost.
- Debugging requires `eprintln!` or Tauri console.
- No correlation between operations and their logs.

## 3. Current state (measured, clean checkout 2026-10-22)

**`tracing` call sites (scanned all crates):**

| Crate | Count | Content |
|-------|-------|---------|
| `manager-telemetry` | ~15 | `tracing::info!`, `tracing::error!`, `tracing::warn!` |
| `manager-app` | ~4 | `tracing::info!` |
| `manager-wizard` | ~1 | `tracing::info!` |
| `src-tauri` | ~0 | none |

**`eprintln!` count:**
- Total: **32** (in `manager-app`, `manager-wizard`, `manager-telemetry`, `src-tauri`).

**Logging infrastructure:**
- `manager-telemetry/src/pdh.rs` has a `BroadcastSink(256)` for events (lossy, 256 cap).
- No `EventSource` (SSE) consumer in UI (25 `setInterval`, 0 `EventSource`).

## 4. Scope

### In scope
- Initialize `tracing_subscriber::fmt::init()` (or `tracing_subscriber::fmt().json().init()`)
  in `src-tauri/src/main.rs`.
- Add a `task_id` field to every `tracing::span!` in `manager-app`.
- Add `tracing::` call sites to replace `eprintln!`.
- Attach logs to the diagnostics export (JSON Lines or SSE).
- Keep `BroadcastSink` for UI event streaming (optional: replace with non-lossy).

### Out of scope (do NOT do)
- Do **not** change `BroadcastSink` capacity — leave it at 256.
- Do **not** change the UI's `setInterval` polling — that is a separate optimization.
- Do **not** remove `eprintln!` from places where it is intentionally used (e.g. `main` panics).

## 5. Implementation plan

### 5.1 Initialize `tracing` in `src-tauri/src/main.rs`

In `main()` or `app.run()`'s `before_init`:

```rust
use tracing_subscriber::{fmt, layer::LayerExt, EnvFilter};

let env_filter = EnvFilter::new(
    std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())
);

fmt::layer()
    .json()
    .with_file(true)
    .with_line_number(true)
    .with_target(true)
    .with_span_events(fmt::format::FmtSpan::ACTIVE)
    .with_env_filter(env_filter)
    .init();
```

### 5.2 Add `task_id` to spans in `manager-app`

In `manager-app/src/lib.rs` (or `ops/` modules), wrap every `op_*` function with a span:

```rust
use tracing::span;

pub async fn op_get_tasks(state: AppState, sink: BroadcastSender<ServerEvent>) -> Result<Value, AppError> {
    let _span = span!(tracing::Level::INFO, "op_get_tasks", task_id = ?Uuid::new_v4()).entered();
    // ...
}
```

Or, if you already have `TaskRecord` in scope, use its `id`:

```rust
let _span = span!(
    tracing::Level::INFO,
    "op_get_tasks",
    task_id = task_record.id.to_string()
).entered();
```

### 5.3 Replace `eprintln!` with `tracing`

Find all `eprintln!` calls and replace them with appropriate `tracing!` macros:

| Context | `eprintln!` → `tracing!` |
|---------|------------------------|
| Error paths | `eprintln!("error: {}", err)` → `tracing::error!("error: {}", err)` |
| Debug hints | `eprintln!("debug: {}", val)` → `tracing::debug!("debug: {}", val)` |
| Warnings | `eprintln!("warning: {}", msg)` → `tracing::warn!("warning: {}", msg)` |
| Info | `eprintln!("info: {}", msg)` → `tracing::info!("info: {}", msg)` |

### 5.4 Feed logs to diagnostics export

Option A (simple): Write JSON Lines to `logs/app.log`:

```rust
use tracing_subscriber::fmt::format::Json;

fn init_logging() {
    // ...
    let file_layer = fmt::layer()
        .json()
        .with_writer(std::sync::Arc::new(FileAppender::new("logs/app.log")))
        .with_filter(env_filter);
    // ...
}
```

Option B (SSE): Attach the logging layer to the SSE broadcast stream (see `BroadcastSink`).

## 6. Definition of done

- [ ] `tracing_subscriber::fmt().json().init()` in `src-tauri/src/main.rs`.
- [ ] Every `op_*` function in `manager-app` has a span with `task_id`.
- [ ] All `eprintln!` replaced with `tracing::` (or documented exceptions).
- [ ] Logs written to `logs/app.log` (or SSE) in JSON Lines format.
- [ ] `cargo test -p manager-app` passes unchanged.
- [ ] `cargo test --workspace` passes unchanged.

## 7. Testing & verification

- Run `RUST_LOG=trace cargo run --package src-tauri`.
- Check `logs/app.log` for JSON Lines.
- Check Tauri console for logs.
- Verify `task_id` appears in every span.

## 8. Risks & pitfalls

- **`tracing` initialization must happen once.** Do not call `fmt::init()` twice (panics).
- **`task_id` must be unique per span.** Use `Uuid::new_v4()` for new tasks, or the
  `TaskRecord.id` if available.
- **JSON format.** Use `.json()` to ensure logs are parseable by tooling (ELK, Loki, etc.).

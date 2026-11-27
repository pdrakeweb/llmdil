# Tiered model storage (hot NVMe / cold SATA)

Status: **approved** by Pete 2026-11-26 with the decisions in §16. Branch:
`feat/tiered-model-storage` (off `main` after fast-forwarding it to
`claude/llmdil-architecture-plan-f50fc6`).

## 1. Goal

Keep rarely used GGUFs on a slow, roomy "cold" drive and keep the models
that matter on the fast "hot" drive, without slowing anything
interactive. On prometheus: hot = `C:` (Samsung 9100 PRO NVMe, holds
`data_dir\models`), cold = `E:\llm-models` (Crucial M550 SATA,
~500–550 MB/s). `F:` is not used.

Non-goals: network tiers,
symlinks/junctions (never), rewriting any model path stored in config,
the catalog or the activation journal.

## 2. Why not "copy when needed" as the only path

Measured shape for a 25 GB model:

| Path | First start | Later starts |
|---|---|---|
| Copy SATA→NVMe, then load from NVMe | ~46 s copy + ~5 s load = **~51 s** | ~5 s |
| mmap-load straight from SATA | **~46 s** | ~46 s |
| Start from SATA now, promote in background afterwards | **~46 s** | ~5 s (from the 2nd start that follows the copy) |

A blocking copy makes the first use slower than just loading from cold
and only pays off from the second start on. So:

- **Start from wherever the file is, immediately.** No start ever waits
  on a copy.
- **Promotion is a background copy** decided by policy (§6). The running
  backend keeps its cold mapping; the next start resolves to hot.
- The copy **waits while any backend is loading** (state Starting), so it
  never competes with a model load for disk bandwidth on either drive.

Constraint 1 (never slow an enabled backend's start) holds by
construction: every model an enabled backend references is pinned hot
(§6.1), and the resolver adds only a few `metadata()` probes to a start.

## 3. Concepts

**Logical path.** What config stores today: `models\X.gguf`, relative to
`data_dir`. It never changes when a file moves. Backend↔model identity
comparisons (the many `resolve_under_data_dir(a) == resolve_under_data_dir(b)`
sites) keep comparing logical paths and are untouched.

**Tierable.** A logical path is tierable when it is relative and its first
component is `models`. Absolute paths and other relative paths are not
tierable and resolve exactly as today.

**Tier.** A root directory that mirrors `data_dir\models`:

- `hot` — always `data_dir\models` (implicit, not configurable).
- `cold` — one or more configured roots, e.g. `E:\llm-models`.
  `models\X.gguf` lives at `E:\llm-models\X.gguf`.

**Resolver.** `resolve_model_path(storage, data_dir, logical) -> Resolved`
probes hot, then each cold tier in configured order, and returns the
first tier where the final file exists (`*.partial` / `*.part` never
count). Result: `{ path, tier: Hot | Cold(i) | Missing, cold_unavailable: bool }`.
When the file is nowhere, `path` is the hot path (today's behavior: the
download destination, the path error messages name). It takes an
`exists` probe so the wizard can pass its `SystemOps` (hermetic tests).
If both tiers hold the file (a promotion whose source delete is
deferred), hot wins.

The resolver lives in `manager-config` (new `storage` module) because
both `manager-app` and `manager-wizard` need it and `manager-config` is
the shared leaf. The mover, policy and Windows I/O live in
`manager-models` (new `tier` module). Orchestration (the reconcile loop,
operations, events) lives in `manager-app` (new `model_storage` module).

## 4. Configuration

New `model_storage` section on `AppConfig`, `#[serde(default)]`, so every
existing `config.json` loads unchanged and behaves exactly as today
(no cold tiers ⇒ the resolver always answers hot).

```jsonc
"model_storage": {
  "cold_tiers": [ { "path": "E:\\llm-models", "label": "Data1" } ],
  "hot_budget_gib": null,          // cap on tierable GGUF bytes in hot; null = no cap
  "hot_min_free_gib": 100,         // keep at least this free on the hot volume
  "promote_after_starts": 2,       // N starts …
  "promote_window_days": 14,       // … within D days ⇒ promote
  "demote_after_idle_days": 30,    // unpinned + not started for this long ⇒ demote; null = never
  "keep_hot": ["qwen_qwen3-5-0-8b-q8_0"],   // operator pins (model ids)
  "download_tier": "hot",          // "hot" | "cold"
  "copy_rate_mib_s": 256,          // copy throttle; 0 = unthrottled
  "auto": true                     // policy moves on/off (manual Promote/Demote always work)
}
```

Validation:

- `manager_config::validate` (runs at load): structural only — cold paths
  absolute, unique, not inside `data_dir`, numbers in range. It must
  **not** probe the disk: a missing `E:` must never stop the app loading.
- `model_storage_set` operation (save time): additionally each cold path
  exists (or can be created), is a directory, and is on a **different
  volume** from `data_dir` (`GetVolumePathNameW` comparison).

## 5. State file: `<data_dir>\model-storage.json`

Not config: it changes on every start and during every move, and must
not churn `config.json` / `.bak` or trip activation drift. Versioned,
written atomically (temp + rename), tolerant of absence/corruption
(starts empty, logged).

```jsonc
{
  "version": 1,
  "usage": { "models\\X.gguf": { "starts": ["2026-11-26T14:00:00Z", …], "last_started": "…", "first_seen": "…" } },
  "moves": [ { "id": "mv-…", "logical": "models\\X.gguf", "from": "hot", "to": "cold:0",
               "phase": "copying|verifying|committed|done|failed", "bytes": …, "error": null, … } ],
  "pending_cleanup": [ { "path": "E:\\llm-models\\X.gguf", "since": "…" } ]
}
```

- `starts` keeps only timestamps within the promotion window (bounded).
- `first_seen` is stamped the first time the feature sees a hot file with
  no history, so idle-demotion's clock starts when the feature is
  installed — nothing is demoted on first launch just because it has no
  history.
- Starts are recorded by every app-mediated spawn: `start_backend`,
  Bench and Optimize temporary backends, supervisor crash restarts.

## 6. Policy

A pure function `plan(inputs) -> Vec<MoveRequest>` in `manager-models::tier`,
run by a reconcile loop in `manager-app` every 60 s and after any config
save, backend start/stop, or move completion. It issues at most one move
at a time (single mover, FIFO).

### 6.1 Pinned (always hot, never auto-demoted)

- referenced by an **enabled** backend's `model_file`;
- whose hot absolute path appears in an enabled backend's `raw_flags`
  (e.g. `--spec-draft-model C:\…\models\Draft.gguf`);
- listed in `keep_hot` (operator pin — the 0.8B Bench draft model lives
  here, since Bench references it from its own store, not a backend).

A pinned model found cold is promoted in the background.

### 6.2 Promotion

Promote a cold, unpinned model when it has `>= promote_after_starts`
starts within `promote_window_days`, or on manual **Promote**. Enabling a
backend pins its model, so the copy starts right away and the start after
the copy lands is hot.

### 6.3 Demotion

Candidates: hot, tierable, unpinned, not open by any running process
(§7.4), no download/verify/move in flight. Triggers:

1. **Idle**: `now - (last_started ?? first_seen) > demote_after_idle_days`.
2. **Pressure**: hot tierable bytes > `hot_budget_gib`, or hot free space
   < `hot_min_free_gib` — demote LRU (oldest `last_started ?? first_seen`
   first) until satisfied.
3. Manual **Demote** (refused for a pinned model, with the reason:
   disable the backend / remove the keep-hot pin first).

A promotion that would breach the reserve first demotes LRU candidates;
if it still does not fit it is skipped and surfaced (alert "hot tier
full"), never forced.

## 7. The mover

### 7.1 State machine (one move)

```
queued ──► waiting (a backend is Starting / I/O gate) ──► copying ──► verifying ──► committed ──► done
   │             ▲          │                    │            │               │
   │             └──────────┘ (pause/resume)     │            │               └─ source delete deferred
   └─ cancelled                                  └──── failed ◄┘                  (in use) → pending_cleanup
```

1. **queued** → checks: source exists and is not `.partial`; no
   download/verify in flight for the model; destination tier available;
   destination free space ≥ size + 2 GiB.
2. **copying**: open the source read-only (sharing with llama-server's
   read mapping is fine), write `<dest>\X.gguf.partial`, hashing the
   source stream (SHA-256) as it goes. 8 MiB buffers. The copying thread
   runs in **Windows background mode**
   (`SetThreadPriority(THREAD_MODE_BACKGROUND_BEGIN)`: very-low I/O
   priority *and* low memory priority, so the copied pages are first out
   of the standby list and do not evict cached pages of hot models).
   Throttled to `copy_rate_mib_s`. Paused while any backend is Starting.
   Cancellable. Progress → task-progress events.
3. Flush + `sync_all` the partial file.
4. **verifying**: re-read the partial file and hash it. Expected hash =
   the model's real pin (64 hex) when it has one, else the source stream
   hash. Both the source stream hash and the destination hash must equal
   it.
5. **committed**: rename `X.gguf.partial` → `X.gguf` (same directory,
   atomic on NTFS); persist `phase: committed` (fsync'd state file).
   If the model has a real pin: re-stamp `verified_at = now`,
   `verified_sha256 = pin` (the copy was just verified against it — the
   start gate stays meaningful and the model is never left unverified
   by a move). Re-register backend spawn specs (and service definitions
   in service mode) so any restart resolves to the new location
   **before** the source disappears.
6. **done**: delete the source. On a sharing violation (still mapped by
   a running/adopted process) the source goes to `pending_cleanup`; the
   resolver already prefers hot, and for a demotion the hot file simply
   stays until it is released. Cleanup is retried on every reconcile
   tick and after every backend stop.

### 7.2 Crash recovery (app start)

- Any `*.partial` in any tier → deleted (copies are not resumable: the
  hash covers the whole stream). Source is intact by construction.
- A move in `copying`/`verifying` → marked `failed (interrupted)`; the
  policy re-queues it if still wanted.
- A move in `committed` → destination exists with the expected size ⇒
  finish step 6. Destination missing ⇒ failed, source kept.
- Both copies present with no move record ⇒ keep both, hot wins, report
  in the Storage card (no automatic delete without a record).

### 7.3 Verification outcomes

| Source stream | Destination | Outcome |
|---|---|---|
| = pin | = pin | commit, re-stamp `verified_at` |
| = pin | ≠ pin | write corruption: delete partial, move failed, source untouched, no gate change |
| ≠ pin | — | **the source itself is bad**: abort, delete partial, keep source, record a verification failure (same gate as a failed Verify: start refused until re-download or a passing Verify) |
| no pin | = source hash | commit (`verified_at` untouched) |
| no pin | ≠ source hash | delete partial, move failed |

### 7.4 "In use"

A model is in use when any live supervised process's `-m` /
`--spec-draft-model` argv names one of its tier paths, any running
service's definition does, or any adopted backend's configured model
resolves to it. This is best effort; the delete-time sharing violation
(§7.1 step 6) is the backstop, so a missed detection can only defer a
delete, never break a running backend.

## 8. Integration points

Filesystem-access sites switch from `resolve_under_data_dir` to the
resolver; identity comparisons stay.

| Site | Change |
|---|---|
| `backend_spec` (`-m`) | resolved path; the one place a launch path is built |
| start preflight | missing-file / cold-unavailable errors name the tier ("model X is on cold tier E:\llm-models, which is not available"); verification gates unchanged |
| `list_models` | `storage: { tier, path, state, move progress }`, size from the resolved file; `.part` probe on the download tier |
| `download_model` | destination = `download_tier` root; in-use guard checks every tier |
| `model_verify_checksum` | verifies the resolved file |
| `model_delete` | deletes the file + partials in every tier |
| Bench draft path, Optimize candidates/experiments/tuning, placement, backend info, GGUF layout cache | resolved path (cache keys on path, so a move is one cache miss) |
| Wizard `DownloadModels` / `ConfigureTensorSplit` | resolver via `SystemOps`; a model found in any tier counts as present (no re-download); a model not found while a cold tier is unavailable is reported, not downloaded |
| Service definitions | regenerated after a move (start already regenerates) |

## 9. Failure cases

| Case | Behavior |
|---|---|
| Power loss mid-copy | `.partial` deleted at next start; source intact; move re-queued by policy |
| Power loss after rename, before source delete | state says `committed` → finish delete on next start |
| Cold drive missing at start | cold tier marked unavailable; hot models unaffected; a cold model's start fails fast naming the drive; Models view shows "Unavailable"; wizard does not re-download it; no moves to/from that tier |
| Cold drive removed mid-copy | I/O error → partial deleted (if reachable), move failed, source kept |
| Model in use | demotion refused up front; if missed, delete deferred to `pending_cleanup` |
| Hash mismatch | §7.3 |
| Disk full | pre-check refuses; `ENOSPC` mid-copy → partial deleted, move failed, alert |
| Start requested during a copy | start proceeds from the current location; copy pauses while the backend is Starting |
| Download or verify in flight | moves for that model refused/queued |
| Config edited to remove a cold tier holding models | `model_storage_set` refuses while that tier holds tierable models (demote/promote them first) |

## 10. Operations (registry)

| Op | Class | Route |
|---|---|---|
| `model_storage_status` | R | `GET /api/v1/model-storage` — tiers (path, available, total/free, tierable bytes), budget, moves, pending cleanup |
| `model_storage_set` | C | `PUT /api/v1/model-storage` — the config section (validated as §4) |
| `model_promote { id }` | L | `POST /api/v1/models/{id}/promote` — returns task id |
| `model_demote { id, tier? }` | L | `POST /api/v1/models/{id}/demote` |
| `model_move_cancel { id }` | L | `POST /api/v1/models/{id}/move/cancel` |
| `model_set_keep_hot { id, keep }` | C | none (PATCH composite, like the other model setters) |

Each move is a task (`model-promote` / `model-demote`) through the
existing task registry, so it shows in task lists, carries `task_id` in
logs, and streams `task-progress`.

## 11. UI (Models view)

Designed against docs/design-principles.md.

- **Storage card** at the top of the Models view: one stacked bar per
  tier (bytes by model, hot budget / free-space reserve drawn as a
  marker) — the visual answer to "where is the space going". Tier paths
  with copy buttons; an "Unavailable" light-red chip when a cold root is
  missing. Moves in flight show as a progress bar with a cancel icon.
- **Model card**: a "Cold" chip (gray) when the file is cold — hot is the
  default, so no chip; "Promoting 42%" / "Demoting 42%" amber while
  moving; "Unavailable" light red. Promote / Demote icon buttons
  (aria-label + tooltip). "Keep hot" is the shared `enableSwitch`.
  Pinned-by-backend shows as the switch disabled-on with a tooltip
  naming the backend.
- **Settings → Model storage** panel: the §4 knobs (form, closes on
  save per rule 17).

## 12. Telemetry and logging

- `tracing` events under target `manager_app::model_storage` (queued,
  started, paused/resumed, verified, committed, cleanup deferred/done,
  failed with reason), each carrying the move's `task_id`, so they reach
  `logs\app.log` and log streaming like every other task.
- Per-backend lifecycle log line on start: `model: <path> (tier cold)`
  when the model resolved to a cold tier.
- `last_started` / start history in the state file (§5); exposed via
  `model_storage_status` and per model in `list_models`.
- Alerts (existing alert channel): hot tier full, cold tier unavailable
  while a model on it is wanted, move failed.

## 13. Tests

- Resolver: hot hit, cold hit, both (hot wins), neither (hot path),
  non-tierable passthrough, unavailable tier, partials ignored, no cold
  tiers = today's behavior.
- Config: old `config.json` without the section loads with defaults and
  round-trips; structural validation; save-time volume/existence checks
  (temp dirs; volume check behind a trait so it is testable).
- Policy: pins (backend, raw flags, keep_hot, disabled backend does not
  pin), N-in-D promotion, idle demotion with `first_seen`, LRU order
  under budget and reserve, never demote pinned/in-use/in-flight,
  promotion that does not fit.
- Mover (temp dirs): happy path both directions, partial file never
  visible to the resolver, pin mismatch at source, destination mismatch,
  no-pin path, cancel, disk-full simulation (writer trait), deferred
  delete, crash recovery for each phase, `verified_at` re-stamp.
- App: preflight message for unavailable cold, `backend_spec` uses the
  resolved path, delete removes all tiers, wizard does not re-download a
  cold model.

## 14. Migration (only after approval)

1. Build + install per docs/installation.md.
2. Settings → Model storage: cold tier `E:\llm-models`; `keep_hot` the
   0.8B draft.
3. Demote the 8 cold models (121.8 GB) through the feature.
4. Confirm coder, coder-fast, planner-moe/bench start at today's speed
   from C:, and Qwen3-8B (5 GB) starts from E:.
5. Report C: free before/after.

## 15. Deferred

- Resumable copies (restart from zero is acceptable at 500 MB/s).
- More than one cold tier in the UI (the config and resolver support it).
- Unbuffered (`FILE_FLAG_NO_BUFFERING`) copies — background memory
  priority is used instead.

## 16. Decisions (Pete, 2026-11-26)

1. **Base branch**: fast-forward `main` to the architecture-plan branch
   (what is installed), then branch from it.
2. **Cold keeps an archive copy.** Promotion copies cold → hot and
   leaves the cold copy in place; demotion of a model whose cold copy is
   intact is an instant delete of the hot copy. A model with no cold
   copy is demoted by copying hot → cold (verified) and then deleting the
   hot copy. Consequences, superseding the move-only wording above:
   - Promotion never deletes anything, so it never defers a delete and
     cannot conflict with the cold mapping a running backend holds.
   - "Intact" means the state file's archive record for the cold copy
     (size, mtime, SHA-256 verified at copy time) still matches the file;
     otherwise the demotion re-copies and re-verifies.
   - Cold holds every model that has ever been demoted; a model that was
     never demoted has no cold copy. The Storage card shows both tiers'
     bytes; a hot model with an archive copy carries an "Archived" mark.
   - `model_delete` removes every tier's copy.
   - Other policy defaults (§4, §6) approved as written.
3. **Unregistered GGUFs** (files under a tier with no model record) are
   listed in the Storage card and can be demoted/promoted. Moves are
   keyed by logical file (`models\X.gguf`), not model id; the model-id
   operations are conveniences over the file operations.
4. **Migration** runs as soon as the feature is committed and green.

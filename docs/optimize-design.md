# Optimize Page — Design

Status: shipped — Phases 0–5 and Addendum A are implemented in
releases through v0.1.40 (core logic in `crates/manager-optimize`;
app glue in `crates/manager-app/src/optimize.rs`,
`optimize_eval.rs`, and `optimize_experiments.rs`). Two v0.1.40
behavior notes: plan-build model grounding (tracker #51) — a
`backend.create` item naming an unregistered model now gets a
chained `model.add` from the catalog or the discovery candidates,
or is excluded with its reason recorded in `Plan.excluded_items`,
instead of simply failing validation (§6's non-resolution rule is
superseded for creates by this grounding); and verbose logging
(tracker #52) — the whole pipeline writes to
`<data_dir>/logs/optimize.log`, rotating at ~1 MiB to
`optimize.log.prev`, served by the Logs view as the `optimize`
scope. Pete's feature spec, 2026-10-17: the app
assembles the system configuration, the software's purpose, and its
configuration and options; sends them to one of the running AI backends;
and asks for optimizations to the layout — which models, which settings
per model, which backends to create, what could be grouped into backends.
The result is an optimization plan: each change carries a tri-state
control (accept / deny / undecided) and the accepted changes are
applyable.

## Key decisions

| Decision | Choice |
|---|---|
| Consultation route | Direct to the chosen backend's port. The gateway is not involved (§3). |
| Consulted backend | Running backends only. The page never starts a backend (§3). |
| Output contract | One JSON object, parsed with per-item salvage (§4). |
| Applyable surface | Structured change kinds only. Wholesale raw-flag rewrites and anything unmapped are advice-only (§4). |
| Apply semantics | Apply saves configuration only, through the existing commands, so every change lands in the Activation Plan. Activation/restarts stay the existing flow (§5). |
| Destructive items | Default state is denied; accepting one is an explicit flip (§6). |
| Plan lifetime | One live plan. A new consultation replaces it (§6). |
| Core logic placement | New workspace crate `manager-optimize` (plan types, parser, validation simulation) so it is tested on Linux CI; src-tauri holds I/O glue only (§7). |

## 1. Page design

### Navigation

New route `#/optimize`, label "Optimize", icon `bolt` (already in
`ui/src/icons.ts`), placed after Gateway and before Activation Plan in
the `ROUTES` table in `ui/src/main.ts`. The page is useless before a
first backend exists, but it is not gated: with nothing running it
renders its empty state (§1.4), which is itself the explanation.

### Layout

One section, two cards, plan first when a plan exists (principles 1–4).

**Consultation card** (always present):

- **Consulted backend**: a select of running backends only. Each option
  is the backend's short model name plus a state chip (rule 11). The
  default selection follows §3. A stopped backend never appears in the
  list; when a stopped backend would have been the default, a line
  names it — "Better model available: planner (stopped)" — as Label:
  Value text with the reason in an info-tip. No start control (§3).
- **Purpose**: a textarea holding the deployment purpose text (§2.3),
  persisted on change. Label "Purpose:"; the placeholder is sample
  text, never a saved value.
- **Context summary**: one line of Label: Value counts describing
  exactly what will be sent — "GPUs: 2 · Backends: 5 · Groups: 5 ·
  Models: 5 · Traffic window: 1 h" — plus an expandable "Context sent"
  holding the exact assembled payload (the same JSON the model
  receives), with a copy button (rule 10). This is the redaction
  audit surface: the operator can read everything that leaves the
  page before sending it.
- **Consult** button: words + icon (`bolt`). Consultation is the page's
  primary operation and occupies the consulted backend for minutes;
  it is operation-critical in the rule-15 sense, so it keeps its word.

**Plan section** (rendered only when a plan exists, rule 13):

- Summary strip: one stacked bar of item states — accepted / denied /
  undecided counts as segments (rule 8) — and per-kind counts as
  chips. No prose summary from the model is rendered as page text;
  the model's own summary line, when present, lives in an expandable
  at the strip's end.
- One row per plan item (§1.3).
- Apply bar: **Apply accepted (N)** button, words + icon (`check`),
  enabled only under the §5 rules; a status line for the last apply's
  outcome counts ("Applied: 4 · Failed: 1 · Not attempted: 2").

### Plan row anatomy

Left to right:

1. **Tri-state control.** One control per row, three states:
   undecided (hollow box), accepted (`check` icon, green), denied
   (`x` icon, light red). Click cycles undecided → accepted → denied
   → undecided. It is a real button with an aria-label naming the
   item and its state ("Accept change 3: coder context"), per rule
   15's accessible-name requirement. Destructive items start denied
   (§6); invalid items have the control disabled (§5).
2. **Change**, stated as Label: Value deltas wherever the change is a
   field change: "coder · Context: 32,768 → 65,536",
   "coder · Offload: 46 → 24 layers", "planner · Placement: A4000
   100% → A4000 50% / P100 50%". Creations and deletions state the
   object and its defining values: "New backend: coder-fast-2 ·
   Model: Qwen3-8B · Port: 8086", "Delete backend: tool-runner".
3. **Kind chip**: the item kind in short form ("Flags", "Placement",
   "New backend", "Delete", "Routing", "Advice").
4. **Validation chip** (§5): "Valid" (green), "Warning" (amber) with
   the warning in a tooltip, "Invalid" (light red) with the reason in
   a tooltip and the tri-state disabled.
5. **Confidence chip**: the model's self-reported confidence
   (high/medium/low), labelled as model-reported in its tooltip. It
   is display only; it never affects validation or defaults beyond
   §6.
6. **Expected effect**: a Label: Value line only when the app computed
   the figure itself during validation — "GPU RAM: 9.3 GiB → 4.1 GiB"
   from the memory-requirement recomputation. When the app cannot
   compute it, the line is omitted (rule 12), never filled with the
   model's prose estimate.
7. **Rationale**: the model's rationale and its prose expected
   effect, inside the row's expandable (rule 5). Nothing else
   expands.

Advice-only items (§4) render identically except: no tri-state
control (a static `info` icon instead), kind chip "Advice", and the
expandable carries the full text. They never count toward Apply.

### States

- **No backend running.** The consultation card renders; the backend
  select is absent (rule 13); one line states "Consulted backend:
  none running" with an info-tip explaining that Optimize consults a
  running backend and does not start one, and a link-styled button to
  the Backends view. Consult is disabled. Purpose remains editable.
- **Consulting.** Consult is disabled and reads "Consulting…" with
  elapsed time; a Cancel button (words + icon, `x`) appears beside
  it. The plan area keeps showing the previous plan if one exists,
  marked stale by a chip ("Plan from 09:12 — consultation running");
  otherwise it renders nothing.
- **Plan returned.** Plan section as above; the consultation card
  collapses its context expandable and shows "Plan generated:
  <time> · Consulted: <backend> · Items: N" as Label: Value.
- **Partially applied.** Applied rows gain an "Applied" chip (green)
  and their tri-state locks. Failed rows gain a light-red chip with
  the failure in a tooltip and stay actionable after revalidation.
  Unattempted rows stay live. The Apply bar counts update. The
  global activation banner (already rendered above the main column
  by `main.ts`) is the path onward to activation (§5.3).

Sample mode follows the existing pattern: `api.ts` returns a sample
plan with `live: false`, so the page is designable without Tauri.

## 2. Input assembly

The context is one JSON document, assembled in Rust from typed
sources through an allowlist builder — never by serializing
`AppConfig` wholesale (§2.4). Target size: under ~12,000 tokens for
realistic installs (Pete's current install assembles to roughly
3,000). Series data is always summarized; raw telemetry samples and
logs are never sent.

### Hardware inventory

- GPUs, from the telemetry store's latest snapshot (the same data
  `get_gpu_telemetry` returns): index, name, VRAM total, VRAM used,
  driver model (WDDM/TCC) where known.
- System RAM total and CPU model/core count: **do not exist anywhere
  in the app today** — the wizard's detect-hardware step covers GPUs
  and driver only. The design adds one small probe at consultation
  time (Windows: `GlobalMemoryStatusEx` + processor information;
  values cached for the session). Until that probe exists, the
  hardware block omits RAM/CPU rather than estimating them — and
  because system RAM decides expert-offload recommendations, the
  probe is a Phase 0 item, not a nicety (§7).

### Configuration

From `AppConfig`, per backend: id, enabled, model id (resolved from
`model_file`), port, `n_ctx`, `n_batch`, tensor split, split mode,
raw flags (verbatim — they are operator configuration, not
secrets), expert offload as resolved by
`manager_models::expert_offload`, and the app's own computed memory
requirement for the backend's current configuration
(`gpu_ram_mib` / `sys_ram_mib` / `kv_ram_mib` + basis, exactly as
`list_backends` already computes them). Including the app's own
figures matters: the model reasons against the same numbers the UI
shows instead of re-deriving them.

Per group: name, members, strategy, fallbacks, and purpose text
(§2.3). Per model: id, short name, quant, params, size on disk,
rated max context (catalog `ctx_max`), on-disk/verified state,
assigned backends. Catalog candidates not yet in the configuration
(the curated catalog's opt-in models) are included as a compact list
(id, name, quant, size, rated context) so "which models" proposals
have something real to reference; a model the consultant invents
outside this list can only arrive as a catalog-addition item (§4)
and is treated as untrusted there.

Gateway: port, bind host, group count, OpenRouter enabled state,
daily cap, and allowlist — never the credential (§2.4). App version
and the consulted backend's identity are included so the model knows
what it is running on.

### Measured state

- Per running backend: state, uptime, and per-process VRAM
  attribution from the latest telemetry snapshot (with `null`
  preserved as "not measured" on WDDM processes the PDH fixup could
  not attribute — rule 12 applies to the payload too).
- Gateway traffic: per-backend trailing-hour requests, errors, and
  throughput from the gateway stats store (`BackendWindow`), when a
  window exists. The store only has data after an app-started
  gateway with metrics enabled has been polled, so the block is
  present-or-absent, and the prompt states which blocks are measured
  and which are configuration-only.
- Telemetry peaks (temperature peaks the store tracks) where
  present.

Nothing else: no request contents, no recent-request log lines, no
prompts the gateway served. The recent-requests data exists in the
app and stays out — it is user traffic, not layout data.

### Purpose and intent

Two tiers:

1. **Deployment purpose** — one persisted, user-editable text on the
   Optimize page: what this deployment is for. Seeded on first use
   with a default assembled from the install itself (group names and
   their models, e.g. "Local inference for software development: a
   planning model, a coding model, and tool-use support, served to
   the LAN through one gateway."), then owned by the user. Stored as
   a new `AppConfig` field `optimize_purpose: String` (serde
   default, empty = unseeded). It is sent verbatim at the head of
   the user prompt.
2. **Per-group / per-backend intent** — what each group and backend
   is *for*. **Nothing captures this today**: `ModelGroup` is
   name/members/strategy/fallbacks and `BackendConfig` has no
   descriptive field. The design adds optional `purpose: String`
   (serde default empty) to both structs, edited where the entities
   already live — an info-tip-adjacent edit affordance on the group
   card and the backend card's Details — and edited in bulk in an
   "Intent" expandable on the Optimize page for convenience. Empty
   intent is sent as absent, and the system prompt tells the model
   to infer cautiously from names when intent is missing rather than
   to assume. Group intent matters most: "what could be grouped" is
   a question about intent, not about flags.

Neither field is a secret; both are the user's own text, sent only
to a local backend.

### Redaction — the hard requirement

The context goes to a process on this machine, but the rule is
absolute anyway: **no secret value is ever placed in the context
document**, so the document is safe to display (§1.2), log, and
persist with the plan. Excluded, by construction of the allowlist
builder:

- The LiteLLM master key (value; it lives in Credential Manager and
  is never read during assembly).
- The PostgreSQL password and any `DATABASE_URL` (contains the
  password). Postgres appears only as "managed PostgreSQL: present,
  port 54329".
- The OpenRouter API key value. The config holds only `cred_ref` —
  a Credential Manager target *name*; the builder omits even the
  name and sends `{enabled, daily_cap_usd, model_allowlist}`.
- Any other Credential Manager material, MXC policy contents, and
  the machine's absolute data-dir path (model paths are sent in
  their configured relative form, which is how `AppConfig` stores
  them).

A unit test in `manager-optimize` assembles a context from a config
seeded with sentinel secret values and asserts none of them appear
in the serialized document (§7, Phase 0). This test is a release
gate for the feature, not a nice-to-have.

## 3. Consultation mechanics

### Which backend

Candidates are backends whose supervisor state is `running` at
consult time. Default selection: the running backend with the
largest rated context (`ctx_max` from the catalog; models outside
the catalog rank by configured `n_ctx`), tie-broken by parameter
count, then by fewest errors in the trailing-hour window. The user
can override with the select; the choice is remembered for the
session, not persisted.

**When the best model is stopped, the page does not start it.**
Starting a backend commits gigabytes of VRAM for minutes of load
time and can evict nothing less than the operator's current working
set; it is an operational act with its own surface (the Backends
view, the dashboard Start All bar). The Optimize page names the
stopped better option (§1.2) and consults the best *running*
backend, or sits in its no-backend state. This is a stated
constraint of the design, not an oversight.

### Direct to the backend, not through the gateway

Recommendation: **direct** — `POST http://127.0.0.1:<backend
port>/v1/chat/completions`, the same loopback address the app
already uses for `test_backend`. Reasoning:

- **Availability.** Optimize is most valuable when the layout is
  wrong, which correlates with the gateway being down, misrouted,
  or not yet started. Backends run independently of the gateway;
  the gateway path would make the diagnostic tool depend on the
  thing being diagnosed.
- **Addressing.** The page chooses a specific backend. The gateway
  routes by group name from the request's `model` field, so
  reaching a chosen backend through it is indirect and depends on
  group membership the consultation itself may propose changing.
- **Auth.** Backends bind loopback and take no key. The gateway
  requires the master key on every request; the app holds it, but
  using it here adds a credential to a path that needs none.
- **Measurement hygiene.** A gateway-routed consultation lands in
  the gateway's own traffic statistics and recent-requests log —
  the very windows §2.3 feeds back as measured state on the next
  consultation. Direct consultation keeps the instrument out of
  its own readings.

Cost of the choice: no gateway fallback if the chosen backend dies
mid-generation. The consultation simply fails with that error and
the user re-runs it; nothing is half-applied, because consultation
never mutates anything.

### Request shape

One non-streaming chat completion. System prompt: the consultant
role, the output contract (§4) stated exactly, the constraint list
(only listed kinds; only ids present in the context; tensor splits
sum to 1.0; at most 12 items; prefer few high-value changes over a
rewrite; every item carries rationale and expected effect; an empty
item list is a valid answer). User prompt: purpose text, then the
context JSON in a fenced block. Temperature 0.2. `max_tokens`
4,096. No `response_format` dependence in v1 — llama.cpp's JSON
modes vary by build, so the contract is enforced by the salvage
parser (§4), with structured-output parameters a Phase 3 upgrade
if a build proves to support them.

Implementation: `test_request` (manager-supervisor) is fixed-shape
(30 s timeout, 16 max tokens, placeholder model) and cannot be
reused as-is. The design adds a sibling, `chat_completion(base_url,
system, user, max_tokens, timeout) -> ChatResult`, in the same
crate, returning content plus usage and latency as `TestResult`
does today.

### Timeouts, cost, latency

Local hardware sets the scale: a 27B model at IQ3_S on one A4000
generates in the tens of tokens per second, so a full 4,096-token
plan can take several minutes; the 80B MoE on the P100 is slower
per token. Consultation therefore runs as a background task in the
app's existing task registry (the pattern `start_backend` already
uses), with:

- Client timeout 900 s (above the wizard's 600 s step budget,
  because generation is the work, not an install step).
- The consulted backend's context must fit prompt + response: the
  assembled context plus 4,096 response tokens is checked against
  the backend's configured `n_ctx` before sending; if it does not
  fit, the consultation refuses with that arithmetic shown, rather
  than letting llama-server truncate the prompt silently.
- Progress is elapsed time plus tokens-so-far when the response
  reports usage at the end; v1 shows elapsed only (non-streaming).
- Cancellation: the existing `task_abort` only flips bookkeeping —
  it cancels no work (known limitation from the capability audit).
  Real cancellation stores the request's abort handle in `AppState`
  and drops the reqwest future; that small addition is part of
  Phase 1, scoped to this command.
- Cost: no monetary cost (local). The real cost is the consulted
  backend's unavailability for other work during generation, which
  the page states in the consulting state ("coder is generating —
  requests to it will queue") when the consulted backend also
  serves a gateway group.

## 4. Output contract

### Shape

The prompt requests exactly one JSON object:

```json
{
  "summary": "one line, rendered only in an expandable",
  "items": [
    {
      "kind": "backend.set_context",
      "target": "coder",
      "change": { "n_ctx": 65536 },
      "rationale": "…",
      "expected_effect": "…",
      "confidence": "high"
    }
  ]
}
```

`kind` and `change` are the contract; `rationale` and
`expected_effect` are prose for the expandable; `confidence` is one
of `high | medium | low` (anything else parses as `low`).
Item identity is assigned by the app on parse (position + a
fingerprint of kind/target/change, used again in §6).

### Malformed and partial output

Salvage, per item — the pattern local models actually produce is
"mostly right with one broken entry", and rejecting a whole plan
for one bad item wastes a multi-minute generation:

1. Extract the first balanced JSON object from the response text
   (code fences and surrounding prose are expected and stripped).
   No extractable object → the consultation fails as a whole; the
   raw response is shown in an expandable with a "No plan could be
   parsed" status. Nothing is applied, nothing is hidden.
2. Deserialize item by item. An item that fails schema validation
   (unknown kind, missing fields, wrong types) is dropped and
   counted: the plan header shows "Items: 9 · Dropped as invalid:
   2" with the dropped raw fragments visible in the same expandable
   as the raw response.
3. A hard cap of 25 items is enforced after parsing; beyond it,
   items are truncated in order and the truncation is shown.

### Kinds and their mapping to real operations

Every applyable kind maps to an existing command with its existing
validation and journal recording. There are no new mutation paths.

| Kind | Target | Change payload | Executes as |
|---|---|---|---|
| `backend.create` | new id | `model_id`, optional `group`, `n_ctx`, `n_batch`, `tensor_split`, `offload_layers` | `backend_add`, then `model_assign`/`placement_apply` for the split |
| `backend.delete` | backend id | — | `backend_delete` (destructive, §5.4) |
| `backend.set_context` | backend id | `n_ctx` | `backend_update_flags` |
| `backend.set_flags` | backend id | `n_batch`, `n_ctx` (structured fields only) | `backend_update_flags` / `backend_apply_config` |
| `backend.set_offload` | backend id | `offload_layers` | `backend_set_offload` |
| `backend.set_placement` | backend id | `tensor_split` (+ `split_mode`) | `placement_apply` |
| `backend.set_model` | backend id | `model_id` | `model_assign` |
| `backend.set_enabled` | backend id | `enabled` | `backend_apply_config` patch |
| `group.set_members` | group name | `members` | `update_gateway_config` |
| `group.set_strategy` | group name | `strategy` | `update_gateway_config` |
| `group.set_fallbacks` | group name | `fallbacks` | `update_gateway_config` |
| `group.create` | new name | `members`, `strategy`, `fallbacks` | `update_gateway_config` |
| `group.delete` | group name | — | `update_gateway_config` (destructive) |
| `model.add` | new model id | `source_url` (https, `.gguf`), `quant`, optional `sha256` | `model_add` (registers only; download stays a Models-view act) |
| `advice` | any | `text` | nothing — advice-only |

Deliberately unmapped in v1:

- **Wholesale `raw_flags` strings.** Raw flags are the largest
  hallucination surface in the app (arbitrary llama-server
  arguments, including ones this build does not know). An item
  whose change is a raw-flags rewrite is rendered advice-only with
  its text preserved. Structured equivalents (offload, context,
  batch) are applyable because the app can validate them.
- **Gateway port / bind host, wizard/install actions, downloads,
  MXC, updates.** Out of the layout question and out of scope;
  items proposing them parse as `advice`.

Advice-only rendering is §1.3's: no tri-state, `info` icon, "Advice"
chip. A parsed item whose kind is known but whose payload fails
the *mapping* (e.g. `backend.set_flags` carrying a `raw_flags`
field) is demoted to advice-only rather than dropped — the
reasoning survives, the mutation does not.

## 5. Validation and apply

### Validation pipeline

Validation runs twice: when a plan is parsed (so rows render with
their chips immediately) and again when Apply is clicked (state
may have moved). It runs in `manager-optimize` against a cloned
`AppConfig` plus a live snapshot (telemetry free VRAM, supervisor
states, catalog). Each item gets one of three states:

- **Invalid** (tri-state disabled, reason shown): target id does
  not resolve; a create collides with an existing id or port; the
  change is a no-op against current configuration ("already
  satisfied" — shown as its own reason, not an error color); two
  accepted-or-undecided items write the same field of the same
  target (the later one is invalid: "conflicts with item N"); the
  simulated post-change configuration fails
  `manager_config::validate`; a placement split does not sum to
  1.0; a fit recomputation (below) cannot place the model.
- **Warning** (selectable, amber chip, reason in tooltip):
  `n_ctx` above the model's rated maximum (mirrors the flags
  editor's existing annotation); a fit that passes only by moving
  another backend's accounted residency; `model.add` (always — the
  source URL and any hash come from the model itself and cannot be
  verified by the app before download; the pinned hash is confirmed
  by the operator in the Models view before any download starts);
  a delete whose target is running (§5.4).
- **Valid**: everything else that passes.

The fit check is the existing machinery, not a new model of the
world: for each item touching model, placement, offload, or
context, recompute the affected backend's `memory_requirement`
under the proposed configuration, then run `fit_check` against
free VRAM per GPU derived from the telemetry snapshot minus the
other backends' requirements — the same accounting the placement
projection already performs. Items are simulated **in plan order**
on the clone, so a create-then-place sequence validates as a
sequence, and the expected-effect line (§1.3) comes from the same
recomputation.

Validation is also where the plan meets reality drift: the plan
records the consultation's timestamp and a hash of the
configuration it was generated against. At Apply, if the live
configuration differs, the apply bar shows "Configuration changed
since this plan" and validation re-runs; items may flip state.
Nothing applies against a stale validation.

### Apply execution

Phase 1 executes from the UI, orchestrating the existing commands
in plan order — the same composition `applyActivationPlan` already
uses — because every mutation's logic (validation, journal
recording, persistence checks) lives in those command handlers
today. A Rust-side executor that duplicates them would create a
second mutation path; consolidating one belongs to the ops-layer
extraction already planned for the admin API, and Optimize should
consume that layer when it exists rather than grow its own (§7).

Rules:

- Only **accepted** items execute. Denied items never execute and
  are never sent anywhere. Undecided items do not execute.
- Execution is sequential, in plan order, and **halts on the first
  failure**: later items were validated against a simulation that
  assumed the earlier ones applied, so continuing would apply a
  plan that was never validated. Outcomes are per-item and final
  for the attempt: Applied / Failed (reason) / Not attempted.
- **No rollback.** Applied items are ordinary configuration saves —
  the same state any manual edit produces — and stay applied. The
  result line says so in Label: Value form ("Rolled back: no").
  Reversal, where wanted, is the Activation Plan's cancel, with its
  existing guard.
- After apply, accepted-and-applied items lock (§1.4); the
  remaining items can be re-validated and applied in a second pass.

### Composition with the Activation Plan

Explicitly: **Optimize proposes configuration changes; the
Activation Plan still governs activation.** Optimize's Apply writes
configuration through the existing commands, each of which records
its journal entry through the existing `record_*` paths — backend
changes as backend-scope entries with their targets, group changes
as gateway-scope entries. Nothing in Optimize restarts a backend or
the gateway. When Apply finishes, the ordinary activation banner is
already showing the pending plan; the operator applies activation
exactly as today (Drain Requests and Apply / Apply now), and
`planApplyLine` describes the restarts from the journal, including
Optimize's entries, without Optimize-specific wording.

A combined "apply and activate in one click" flow is **not**
recommended for v1: the separation between saving a change and
interrupting serving processes to activate it is the Activation
Plan's reason to exist, and an AI-proposed plan is the last place
to collapse that separation by default. The banner is one glance
away on the same screen; nothing is lost but a click.

### Destructive items at apply

`backend.delete` executes as stop-then-delete when the target is
running (the delete command refuses a running backend, so the stop
is part of the change the item describes; its expected-effect line
says "Backend will be stopped and removed" before anyone accepts
it). `group.delete` needs no process action. Both are default-denied
(§6) and carry the destructive styling. Whether the stop-then-delete
composition is acceptable, or deletes of running backends should be
invalid until the operator stops the backend manually, is a
decision for Pete (§8).

## 6. Risks and limits

- **Hallucinated settings.** Mitigated structurally: the kinds are
  a closed set, fields are typed, ids must resolve against the
  context the model was given, and raw-flag rewrites are
  advice-only. A model cannot invent a flag the app will execute,
  because the app never executes flag text from the plan.
- **Hallucinated models.** `model.add` is the one place new
  external facts enter. It is warning-always, default-undecided,
  register-only (no download), and its hash — if supplied — is
  treated as unverified text the operator confirms out-of-band
  before downloading. A model id outside the catalog and outside
  a `model.add` item fails validation by non-resolution.
  (v0.1.40, tracker #51: for `backend.create` items the plan
  builder grounds the reference first — it chains a `model.add`
  from the catalog entry or a discovered candidate ahead of the
  create, and a create whose model resolves nowhere is excluded
  with its reason in `Plan.excluded_items` rather than offered.
  See `ground_create_models` in
  `crates/manager-optimize/src/parse.rs`.)
- **Over-eager rewrites.** The prompt asks for at most 12 items and
  prefers minimal change; the parser caps at 25; no-op items are
  filtered by validation; conflicts invalidate the later item.
  There is no "accept all" control — bulk acceptance of an
  AI-generated rewrite is exactly the failure mode the tri-state
  exists to prevent. (Accept-all for *valid, non-destructive* items
  can be debated later; v1 omits it deliberately.)
- **Destructive defaults.** `backend.delete` and `group.delete`
  items start **denied**, not undecided. Accepting one is a
  deliberate flip past the denied state. Deletes also cannot be
  made valid by confidence: a `high`-confidence delete is still
  default-denied.
- **Repeat consultation.** A new consultation **replaces** the
  live plan; versioning is not kept. If the current plan carries
  any accept/deny decisions or applied items, Consult asks first
  ("Replace the current plan?"). Denied items are remembered for
  the plan's lifetime; across consultations, Phase 2 remembers
  denial *fingerprints* (kind + target + change hash) with the
  persisted plan state, so an identical re-proposal arrives
  pre-denied with a "Previously denied" marker. Fingerprint memory
  is a convenience, never a block: the operator can flip it.
- **Stale measurements.** The plan is generated against a snapshot;
  §5's revalidation at Apply is the guard. A plan left open across
  a hardware change (the second P100 appearing, say) is simply
  revalidated — items may flip, none apply on stale arithmetic.
- **The consultant's blind spots.** The model sees configuration
  and summaries, not logs or failure history; its proposals about
  reliability are inference from error counts. The rationale is
  always visible before acceptance, and validation — not the
  model's confidence — is the gate.
- **Self-reference.** The consulted backend may propose changes to
  itself, including its own deletion. That is allowed (it is a
  layout question) and handled by the ordinary rules: its deletion
  is default-denied, and applying any change to it takes effect at
  its next start through the Activation Plan, not mid-consultation.

## 7. Implementation sketch

### New code

- **`crates/manager-optimize`** (new workspace crate, Tauri-free):
  plan item types and serde; the context document builder
  (allowlist, redaction-tested); the prompt builder; the salvage
  parser; the validation simulation (uses `manager-config` validate
  + `manager-models` requirement/fit, as §5). All of it unit-tested
  on Linux CI — the crate placement is deliberate: src-tauri is a
  binary crate outside the workspace, and this feature's core logic
  should not inherit that blind spot.
- **manager-supervisor**: `chat_completion` sibling to
  `test_request` (§3).
- **Hardware probe**: total RAM + CPU description, read once per
  session (Phase 0; small, Windows-first with a Linux stub for CI).
- **src-tauri**: context assembly I/O (telemetry snapshot, stats
  windows, supervisor states), the consult task with its abort
  handle, and command glue. Config additions: `optimize_purpose` on
  `AppConfig`, `purpose` on `ModelGroup` and `BackendConfig` (all
  serde-defaulted; no migration).
- **UI**: `#/optimize` route; the view in `static.ts` (or a new
  `views/optimize.ts` if the file's size argues for it — it is
  already the largest view file, so a new file is the better
  default); `api.ts` wrappers with sample-mode fallbacks; a sample
  plan in `mock.ts`; the tri-state control as a shared component.

### Commands

| Command | Purpose |
|---|---|
| `optimize_get_state` | Purpose text, last plan (if persisted), running-backend candidates with the computed default |
| `optimize_save_purpose` | Persist the deployment purpose (group/backend intent edits ride the existing config-save paths) |
| `optimize_consult` | Assemble context, run the consultation as a task, parse, validate, store the plan; returns the plan with validation states |
| `optimize_validate_plan` | Re-run validation for the stored plan against live state (Apply-time and on demand) |
| `optimize_discard_plan` | Drop the stored plan and its decisions |

Apply has **no new command** in Phase 1 (§5.2). The plan and its
tri-state decisions persist server-side in
`<data_dir>/optimize-plan.json`, written by the consult/validate
commands and updated by an `optimize_set_item_state` command — the
activation journal is the precedent: an unapplied plan must
survive an app restart, and decisions are the operator's work.

### Phases

0. **Foundations.** Config fields (purposes), hardware probe,
   context builder + redaction tests, plan types + parser in
   `manager-optimize`. Nothing user-visible.
1. **Consult and review.** `chat_completion`, `optimize_consult`
   with real cancellation, the page (consult card, plan rows,
   tri-state, validation chips), persisted plan. Apply via UI
   orchestration of existing commands; activation composition as
   §5.3.
2. **Memory.** Denial fingerprints across consultations,
   previously-denied markers, plan staleness surfacing.
3. **Polish, if wanted.** Streaming generation progress,
   structured-output parameters where the llama.cpp build supports
   them, a Rust-side apply executor on top of the admin API's
   ops-layer extraction when that lands.

## 8. Decisions for Pete

1. **Activation composition** — two-step (Optimize applies config;
   the existing banner/Activation Plan activates) vs a combined
   apply-and-activate button on the Optimize page. *Recommendation:
   two-step (§5.3).*
2. **Destructive defaults** — deletes default-denied, and
   stop-then-delete as part of applying a running backend's delete
   item, vs deletes of running backends invalid until manually
   stopped. *Recommendation: default-denied plus stop-then-delete,
   stated on the item (§5.4).*
3. **Intent capture** — persisted purpose fields (deployment text +
   optional per-group/per-backend purpose) vs page-session-only
   text. *Recommendation: persisted fields (§2.3); the quality of
   every consultation depends on them.*
4. **Catalog additions** — `model.add` items allowed (warning-always,
   register-only) vs advice-only in v1. *Recommendation: allowed as
   specified (§4, §6); the download decision stays human and
   elsewhere.*
5. **Plan replacement** — new consultation replaces the plan (with
   a confirm when decisions exist) vs kept versions. *Recommendation:
   replace (§6).*
6. **Consultation route** — direct-only in v1 vs a gateway option.
   *Recommendation: direct only (§3).*

---

# Addendum A — Measurements, experiments, directives, and discovery (tracker #28)

Status: Pete-directed Oct 17, 2026; **shipped as Phase 5**
(sub-phases 5a–5e, in releases through v0.1.40: benchmark runner
and fingerprint-keyed measurement store at
`<data_dir>/optimize-measurements.json` with median-of-3 figures,
directive capture, HF discovery pre-pass, screening suite, and the
experiment runner using temporary `opt-exp-*` backends under the
§A.3 budgets — at most 3 per batch, 30 minutes each, 60 minutes
per batch — and never downloading models). Phase placement:
**Phase 5**, after the
apply executors (the current build's Phase 4), as its own phase
with sub-phases (§A.7). The current build reserves what this
addendum needs: the context schema carries a `measurements`
section (absent until the Phase 5 store exists), and the
plan/validation layer treats evidence as typed inputs rather than
assuming all evidence is static.

Motivation: the consultation today reasons from catalog data,
computed requirement figures, and telemetry snapshots. It has no
access to how fast a model actually runs on this machine. Pete
measured his planner by hand at 21.8 tok/s generation (direct to
the backend, llama-server timings); that number exists nowhere in
the app. And when the right answer is "a different model", the
consultation can only propose from what the catalog already
knows. This addendum closes both gaps: measure what runs, and
experiment — under approval, with cleanup guaranteed — on what
might run better.

## A.1 The measurements model

### What is measured

Per backend configuration, from llama-server's own `timings`
object on chat-completion responses (llama.cpp reports
`prompt_per_second`, `predicted_per_second`, and the underlying
millisecond/token counts on every completion):

- `prompt_per_second` — prompt-processing rate.
- `predicted_per_second` — generation rate (the tok/s figure).
- Time to first token: not in the non-streaming timings; the
  benchmark runner issues its runs with streaming on and records
  the time to the first content chunk. When streaming fails, the
  TTFT field stays absent — never estimated.

### The standardized benchmark

Fixed prompts and budgets, so numbers are comparable across
configurations and over time:

- **Short prompt** (~200 tokens: a fixed paragraph plus a
  question), generate 256 tokens.
- **Long prompt** (~4,000 tokens: fixed filler with a question at
  the end — exercises prompt processing and KV at depth),
  generate 128 tokens.
- One warm-up run per prompt, discarded (first-run effects after
  a model load are not the configuration's steady state). Then
  three measured runs; the stored figure is the **median**.
- Temperature 0. Runs are sequential, one backend at a time —
  a benchmark never shares the GPU with another benchmark.

### Persistence and invalidation

Results persist in `<data_dir>/optimize-measurements.json`,
keyed by a **config fingerprint**: a hash over the model's file
identity (verified sha256 when stamped, else file size), quant,
`n_ctx`, `n_batch`, resolved offload layers, tensor split, split
mode, normalized `raw_flags`, and the GPU set (indices + names).
A configuration change produces a different fingerprint, so a
stale measurement is never presented as current — it simply is
not found under the new key. Old entries are retained (they are
the delta history a follow-up consultation reasons over) and are
labeled superseded when surfaced.

Each entry: fingerprint, backend id, model id, timestamp, the
short/long prompt and generation rates, TTFT, the per-run
numbers (not just the median), and the app version.

### Into the context

The context document's `measurements` section (schema reserved
in the current build) carries, per backend with a current-
fingerprint entry: the rates, TTFT, `age_secs`, and
`basis: "measured"`. The labeling discipline of §2 applies:
measured outranks catalog, which outranks estimate, and the
system prompt instructs the consulted model to prefer measured
figures when both bear on a proposal. Entries under a superseded
fingerprint for the same model may be included, labeled as such,
when a change is being evaluated — that is what lets a follow-up
consultation say "the change cost 2 tok/s" instead of guessing.

### Who measures, in Phase 5a

- An operator-triggered **Measure** action for a running backend
  (no lifecycle change; the backend serves two benchmark prompts
  between real requests).
- Experiment runs (§A.3) measure their temporary backend as part
  of the run.

## A.2 The operator directive

The consultation gains a free-form **directive**: a first-class
input, distinct from the standing purpose (§2.3). Purpose says
what the deployment is for; a directive scopes one consultation
— "I want to explore better MoE capable models for the planner",
"reduce coder latency", "the tool-runner feels slow".

- **Capture (UI):** a text field on the consult card, optional,
  empty by default, labeled "Directive (optional)". Editing it
  never touches the persisted purpose fields.
- **Carriage:** the user prompt gains a labeled block directly
  after the purpose: the directive verbatim under "Operator
  directive for this consultation:". The system prompt's framing
  narrows accordingly: proposals serve the directive; layout
  changes unrelated to it belong in advice at most.
- **Persistence:** the directive is stored verbatim on the plan
  it produced (`optimize-plan.json`), shown on the plan header,
  and offered as the default text of the field for the next
  consultation (editable, clearable) — an iterate-on-results loop
  keeps its scope without retyping.
- **Experiments:** an experiment proposal must name the
  directive (or the purpose need) it serves; a proposal serving
  neither is advice, not an experiment.

The `Plan` type gains the field additively (serde-defaulted), so
plan files written before Phase 5 stay readable.

## A.3 Experimental backends

An **experiment** tests a candidate model/configuration on the
real machine and records measured results. It is proposed by a
consultation, approved by the operator, executed by the app, and
cleaned up unconditionally.

### Proposal shape

Experiments are **not** plan items: the 15-kind contract (§4)
mutates configuration permanently, while an experiment is
ephemeral, measured, and self-cleaning — mixing the two would
muddy apply ordering, fingerprints, and outcomes for both.
Instead the consultation output gains a sibling top-level array,
parsed with the same salvage discipline (per-entry drop/demote,
cap 3):

```json
"experiments": [{
  "candidate_model_id": "...",        // registered, catalog, or an HF repo+file from discovery (§A.5)
  "flags": { "n_ctx": 32768, "offload_layers": 40, "tensor_split": [0.5, 0.5] },
  "serves": "directive or purpose need, in words",
  "hypothesis": "what should improve and why",
  "predicted": { "predicted_per_second": 35.0, "fit": "alongside running backends" },
  "requires_stopping": [],            // backend ids; default empty
  "run_eval": true
}]
```

Each proposal gets the same tri-state discipline as plan items
(accept / deny / undecided — undecided never runs), rendered in
its own plan section. Deletes' precedent applies to
`requires_stopping`: naming a backend to stop is a consequential
claim, starts denied, and is stated on the proposal.

### Execution

Operator-accepted experiments run one at a time, as one tracked
task under the Phase 1 scoped-cancellation handle:

1. **Preflight.** The candidate model file must already be on
   disk. Experiments never download: a missing file fails the
   experiment with "download in Models first" (downloads are
   tens of GB and stay a deliberate Models-view act — the same
   line `model.add` draws). Fit is checked with the existing
   machinery (`memory_requirement` / `fit_check`) against live
   free VRAM plus the residency of backends that stay running.
   The preflight verdict is stated in numbers: fits alongside
   what is running, or names exactly what it needs stopped —
   and stopping happens only for ids the operator accepted in
   `requires_stopping`.
2. **Create.** A temporary backend `opt-exp-<n>`, loopback only,
   created **outside every gateway group** (see decision A.8.1:
   the stock `backend_add` always joins a group, so Phase 5
   builds a small dedicated runner in the src-tauri glue that
   registers the backend and its supervisor spec directly —
   a backend in no group is structurally valid). It never
   enters gateway configuration, fallbacks, or routing.
3. **Run.** Start via the existing start machinery, wait for
   healthy on the existing probe, run the standardized
   benchmark (§A.1), then the eval suite (§A.4) when `run_eval`.
4. **Record.** Benchmark results go to the measurement store
   under the temporary configuration's fingerprint; eval results
   to the experiment record. Both survive cleanup — the results
   are the point.
5. **Cleanup — guaranteed.** Stop + delete the temporary
   backend on success, on failure, on timeout, and on
   cancellation (cleanup owns the temp id independently of the
   cancelled future). A startup sweep deletes any leftover
   `opt-exp-*` backends, so even an app crash mid-experiment
   leaves nothing behind.

### Iteration

After a batch, the page offers **Consult with results**: a
follow-up consultation whose context now carries the fresh
measurements and eval scores. The model can then propose the
next experiment, or a real change (a plan item) adopting a
winner. Iteration is operator-paced — each round is its own
approved consultation and batch; there is no autonomous loop in
this phase.

### Budget (recommendation, decision A.8.2)

- At most **3 experiments per batch** (the parse cap matches).
- At most **30 minutes wall-clock per experiment**, load and
  eval included; at most **60 minutes per batch**.
- Benchmark and eval token budgets per §A.1/§A.4.

## A.4 Quality equivalence — the screening suite

"Equivalent or better capability at higher tok/s" cannot rest
on tok/s. The experiment therefore also runs a small fixed
**evaluation suite** — identical tasks, identical settings
(temperature 0, per-task token caps), against the incumbent
backend first (the baseline; cached while the incumbent's
config fingerprint is unchanged), then against each candidate.

### Suite shape (v1: ~12 tasks, shipped with the app, versioned)

Checkable outcomes only — every task scores by machine, never by
a model's judgment:

- **Instruction following:** format-constrained outputs (exact
  JSON shape, exact line count) — regex/parse checks.
- **Factual recall:** short questions with exact-match answers.
- **Arithmetic/reasoning:** numeric answers, exact match.
- **Code-adjacent:** structured transformations and extraction
  with exactly checkable results. (Executing generated code is
  **out** for v1 — decision A.8.4.)
- **Summarization:** fixed passage, keyword-coverage check.
- **Tool-call formatting:** emit a tool call for a described
  function — schema-validated (directly relevant to the
  tool-runner seat).
- **Long-context retrieval:** a needle question over the §A.1
  long prompt.

### Reporting and budgets

Scores are reported **beside** the speed numbers, never merged
into one figure: candidate | generation tok/s (short/long) |
prompt tok/s | TTFT | screen overall and per-category | delta
vs baseline. Budgets: ≤2,048 output tokens per task, ≤~30k
tokens per backend for the whole suite, 10 minutes per backend
time-box.

### The caveat, stated wherever scores appear

> Screening suite — a small fixed set of checkable tasks. It
> screens out regressions; it is not a benchmark and does not
> rank models generally.

The follow-up consultation is instructed that a replacement
claim requires: screen overall at or above baseline, no
category at zero that the baseline passes, and higher measured
speed. The app presents the evidence; it does not gate the
plan — the operator decides, as everywhere in this app.

## A.5 Research-augmented candidate discovery

A directive like "explore better MoE models for the planner"
needs candidates the local catalog does not carry. Three ways
to research them:

**(a) Tool-loop consultation.** The consulted model gets
search/fetch tools mid-completion (a delegation-engine-style
sub-loop inside `optimize_consult`). Single interaction, model-
driven — but it bets the feature on the tool-calling reliability
of whichever local model is consulted, which is precisely what
the gateway-MCP design's Phase 0 probes exist to verify;
multi-minute loops and tool schemas land in the consultation
path; failure modes multiply.

**(b) Structured pre-pass (app-side, deterministic).** When a
directive is present, the app gathers candidates *before* the
consultation: HF API search seeded from the directive and the
target seat's role, filtered to GGUF repos; for each of the top
candidates, the HF model record (architecture, parameter count,
rated context, every quant file with its exact size); fit
computed locally with the existing evaluator per quant. The
result is a `candidates` context block — vetted, fit-checked,
real numbers — and the consultation reasons over it. The model
never browses. Deterministic, testable, works with any consulted
model; breadth is bounded by HF search quality, and the operator
sees the candidate block before consulting and can prune it.

**(c) Gateway MCP surfaces.** Research through the gateway MCP
build's tools (tracker #23: Brave search, SearXNG, `web_fetch`).
Real web reach — release posts, third-party benchmarks — but it
depends on #23 landing and its probes passing; the app would
become an MCP client of its own gateway; and results are
unstructured text the consulted model must sift, inflating
context and variance.

**Recommendation: (b) for Phase 5 v1.** Its only dependency is
the HF API, which the app already integrates for downloads.
(c) may later augment the pre-pass with announcement/benchmark
color — same pre-pass shape, one more source, labeled web
(unverified). (a) is explicitly not in v1; revisit only if a
consulted model class demonstrates reliable tool use in
measurement.

The `candidates` block is distinct from `catalog_candidates`
(curated, local) and carries provenance per candidate (HF repo
id, retrieved-at). Directive-less consultations skip the
pre-pass entirely.

## A.6 Model-data sources — evaluation

### canirun.ai (Pete's suggestion)

Evidence (checked Oct 17, 2026): a client-side browser tool —
**110 models, 309 devices** (its homepage's own counts), curated
by hand. Per-model pages carry genuinely useful tables (VRAM
per quant, MoE active-parameter data, benchmark tables copied
from model cards), and its methodology page is candid: tok/s is
a **bandwidth formula** (bandwidth ÷ model size × an efficiency
factor), self-described at ±20%; its data is itself gathered
from the HuggingFace API, the Ollama library, model cards, and
vendor datasheets. There is **no public API**: no developer
docs, no keys; `robots.txt` explicitly disallows `/api/`.
Two structural mismatches settle it: its figures are estimates
(this addendum exists to replace estimates with measurements),
and its fit model assumes a whole model in one consumer GPU's
VRAM — it has no notion of tensor splits across GPUs or expert
offload to system RAM, so its verdicts would be wrong for
exactly the configurations Prometheus runs most (the coder's
43.5 GiB of experts live in system RAM by design).
**Verdict: not a programmatic source.** At most a human
cross-check in a browser. An MCP wrapper would mean scraping a
JS app against its robots posture for estimate-grade data the
app computes better itself. Not recommended (decision A.8.5).

### Hugging Face API

Evidence (verified live, Oct 17, 2026): `GET /api/models/{id}`
returns a `gguf` block — total parameter count, architecture,
rated context length, total file size — plus every sibling file
(with `?blobs=true`, exact sizes and LFS hashes), card data,
downloads/likes, and base-model tags, in one call.
`GET /api/models?search=…&filter=gguf&sort=downloads` drives
discovery. No auth for public models. These are the same facts
the app derives from GGUF headers after a download — available
*before* committing to tens of GB. **Primary structured source:
discovery and fit facts.**

### General web search + fetch (gateway MCP, tracker #23)

Unstructured and unverifiable as a data feed — numbers must be
extracted by a model and carry no provenance label worth
trusting — but the right tool for release announcements,
third-party benchmark write-ups, and known-issue reports.
**Role: corroboration and color in the pre-pass, labeled web
(unverified); never a fit-data source.**

### Recommended stack

HF API (discovery + fit facts) → the app's own fit evaluator
(fit) → the app's own measurement store (speed truth) → web via
the gateway MCP surfaces (optional color, later). **No new MCP
server for model data in v1** — the pre-pass calls HF REST
directly from Rust. If a model-data MCP server is ever wanted
(e.g. for gateway-attached agents), it belongs in tracker #23's
curated registry as another `AppConfig.mcp.servers` entry — no
parallel path. Sketch, for that eventuality: server id
`model_data`, tools `search_gguf_models(query, max_results)`,
`model_facts(repo_id)`, `fit_estimate(repo_id, file,
hardware_json)` — a thin HF wrapper in the same shape as
`web_fetch`.

## A.7 Phase placement

Phase 5, after the current build's Phase 4 (apply executors),
gated like every other phase, Pete's approval of this addendum
before it starts:

- **5a** Benchmark runner + measurement store + context
  integration + Measure action for running backends.
- **5b** Directive capture (UI, prompt carriage, plan
  persistence).
- **5c** Discovery pre-pass (HF client, `candidates` block).
- **5d** Screening suite + baseline protocol + results
  reporting.
- **5e** Experiment proposals (parse/render) + the experiment
  runner + guaranteed cleanup + Consult-with-results.

(5b is small and could be pulled earlier on Pete's word; as
designed it rides with Phase 5.)

## A.8 Decisions for Pete (addendum)

1. **Experiment placement** — temporary backends outside
   gateway groups via a small dedicated runner (requires
   building that runner in Phase 5e) vs a throwaway
   `optimize-experiments` group via the stock commands.
   *Recommendation: outside groups — a group member is
   reachable through the gateway, which an unvetted candidate
   must not be.*
2. **Experiment budget** — 3 per batch, 30 min each, 60 min
   per batch. *Recommendation: as §A.3.*
3. **Discovery mechanism** — HF structured pre-pass for v1
   (§A.5). *Recommendation: (b).*
4. **Screening suite scope** — ~12 checkable tasks as §A.4,
   no code execution in v1. *Recommendation: as §A.4.*
5. **canirun.ai** — no integration (§A.6). *Recommendation:
   none; HF API is the source.*

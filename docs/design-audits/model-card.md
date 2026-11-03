# Design audit — Model card

- **Card:** Model card, Models view
- **Source:** `modelCard`, `ui/src/views/static.ts:2066` (helpers `modelShaBadge` :2026, `hashPinned` :2022; `ModelRow` at `ui/src/api.ts:830`)
- **Audited against:** `docs/design-principles.md`, Part 3 checklist (every answer must be "no")
- **Verdict:** Redesign needed — 11 of 14 checklist items fail
- **Line numbers** are from the Oct 16, 2026 snapshot; `static.ts` is under concurrent edit by the Gateway/Logs UI pass, so lines may drift a few lines either way.
- **Post-v0.1.40 note (2026-10-18):** the checklist now runs to 16 questions, and the card's Enable/Disable action is no longer a button — it is the shared two-position state switch (`enableSwitch`, `ui/src/switch.ts`; tracker #48, design rule 16). Mockups below that show a [Disable] button are the pre-switch design.

## Scorecard

| # | Checklist question | Result | Evidence |
|---|---|---|---|
| 1 | Compact — anything that belongs behind a toggle/expandable? | **Fail** | Truncated sha256 line (`static.ts:2135`) and gateway stats line (`:2136`) are always-visible detail; the rename panel (`:2114`) shows the correct toggled pattern exists in the same card |
| 2 | One section, multiple views — same data two ways in the card? | Pass | No duplicated representation inside the card (duplication of a number is scored at #6) |
| 3 | Prime real estate — top element not the answering value? | Pass | Header is name + quant chip + integrity badge (`:2126–2128`) — identity and integrity are this card's question |
| 4 | Narrative in tooltips — explanation visible inline? | **Fail** | Verifying sentence (`:2094`), partial-download sentence (`:2096`), "no metrics window yet" (`:2136`) |
| 5 | Real data — any visual not corresponding to a value? | Pass | The card has no bars/fills/gauges; chip colors encode state (semantics at #11) |
| 6 | Numbers in one place — any number shown twice? | **Fail** | Partial bytes in the Size value (`:2122–2124`) and again in the Paused span (`:2084`) and Partial span (`:2096`) |
| 7 | Density with order — grouped by something other than its question, detail before summary? | **Fail** | Hash detail (`:2135`) and stats (`:2136`) sit between the summary grid and the actions; the operative state is buried in the bottom actions row (`:2084–2101`) |
| 8 | Multi-number data is a chart — related numbers as text? | **Fail** | Download progress (partial vs total) is text only (`:2084`, `:2088`, `:2096`); gateway stats triple is a text run (`:2136`); `meter`/`stackedBar` exist at `ui/src/charts.ts:81` / `:62` and are unused here |
| 9 | Label: Value — any sentence instead of a labeled value? | **Fail** | State spans are sentences (`:2094`, `:2096`); stats line is a prose run under a parenthetical label (`:2136`) |
| 10 | Copy buttons — any URL/key/address without one? | **Fail** | sha256 truncated to 16 chars, no copy button (`:2135`); GGUF file path, no copy button (`:2131`); no copy affordance anywhere in the card |
| 11 | Status colors — any break of green/gray/light-red/amber? | **Fail** | "disabled" chip is `badge-warn` → amber `#d29922` (`:2126`; `ui/src/styles.css:384`, `:15`) — disabled is gray under principle 11, amber is transitional only; "hash not pinned" is also amber (`:2033`) for a static state |
| 12 | Unknown is not zero — unmeasured rendered as 0? | **Fail** | `stats.err ?? 0` and `trim1(stats.toks ?? 0)` (`:2136`) render absent measurements as "0 errors" / "0 tok/s" |
| 13 | Empty states — placeholder prose instead of omission? | **Fail** | "no metrics window yet" printed when a serving backend has no window (`:2136`); "sha256: —" line printed when no hash exists (`:2135`) |
| 14 | Number formatting — >1 decimal, trailing `.0`, decimals ≥100? | **Fail** | `${m.params_b}B` interpolates the raw float (`:2133`) — no `trim1`; every other number routes through `fmtBytes` (`:91`), `trim1` (`charts.ts:132`), or `toLocaleString` (`:2136`) |

**Gap count: 11** (items 1, 4, 6, 7, 8, 9, 10, 11, 12, 13, 14). Passes: 2, 3, 5.

## Findings in detail

### Narrative inline (items 4, 9) — the top gap

The card's state is expressed as prose sentences inside the actions row:

- `:2094` — "Verifying checksum… — the download finished and the file is being checked against the pinned SHA-256."
- `:2096` — "Partial download · 12.3 GB of ~49 GB total (estimate) — the file is incomplete and cannot be used yet"
- `:2136` — "Gateway (1h, 2 backends): 1,234 requests · 3 errors · 45 tok/s", or the fallback sentence "no metrics window yet".

The one part of the card that is already compliant is the File/Size/Parameters grid (`:2130–2134`), which uses the `.kv` Label: Value pattern. Everything below it regresses to sentences.

Note on the "description blurb": there is no description field on `ModelRow` (`ui/src/api.ts:830–870`) and `modelCard` renders no model description. The narrative in this card is the state sentences above — that is the text to eliminate.

### Download progress has no visual (item 8), and its numbers repeat (item 6)

A download is the one dynamic, multi-number state this card owns — bytes downloaded vs total — and it is rendered three times as text and zero times as a bar: Size value "12.3 GB (partial)" (`:2124`), Paused span "Paused · 12.3 GB" (`:2084`), Partial span "Partial download · 12.3 GB of ~49 GB total (estimate)" (`:2096`). The Downloading span (`:2088`) shows bytes with no total at all, so progress is not even computable from what is visible in that state.

### Status color semantics (item 11)

- Disabled renders amber: `<span class="chip badge-warn">disabled</span>` (`:2126`), and `.badge-warn { color: var(--warn) }` with `--warn: #d29922` (`styles.css:384`, `:15`). Principle 11: gray = disabled/neutral; amber = starting/stopping transitional states only.
- "hash not pinned" is also amber (`:2033`) — a static configuration state, not a transition.
- The transitional states that *should* be amber (downloading, paused, verifying) have no chip at all — their color lives in unclassed prose spans (`:2084–2096`, `color: var(--text2)`).
- "pinned, not verified" is a plain neutral chip (`:2035`) while the Models overview strip treats the same state as warn — the two surfaces disagree.

### Integrity value is truncated and uncopyable (item 10)

`:2135` prints `sha256: ` plus the first 16 hex chars and an ellipsis. The full hash — the one value in this card a user would paste into a verification command — is nowhere in the card and has no copy button. The file path (`:2131`) likewise has no copy button.

### Unknowns and empty states (items 12, 13)

- `:2136`: when a metrics window exists, missing fields coerce to zero (`?? 0` twice). A window with requests but no recorded error field reads "0 errors", asserting a measurement that was not made.
- `:2136`: when backends serve the model but no window exists, the card prints the placeholder sentence instead of omitting the block (contrast the backend card's "Gateway stats: —" treatment).
- `:2135`: with no hash, the line still renders as "sha256: —".
- Done right, for contrast: the catalog size is honestly labeled "(estimate)" everywhere it appears (`:2096`, `:2101`), and `api.ts:862–864` documents it as display-only.

### Formatting (item 14)

`:2133` — `${m.params_b}B` is the only number in the card not passed through a formatter. A `params_b` of 30.55 renders "30.55B" (two decimals); a value ≥100 keeps decimals. `trim1` (`charts.ts:132`) is the house formatter and is already imported by this file.

### Order (items 1, 7)

Reading order today: identity → id sub-line → File/Size/Parameters → sha256 fragment → gateway stats → actions (which carry the state) → rename panel → error → note. The state — the thing that decides which actions make sense — is last and written as prose; two detail lines (hash fragment, stats) precede it. The rename form (`:2114`) is correctly a toggled panel; nothing else on the card is collapsible.

## Redesign proposal

Presentation only — same data, same actions, same `ModelRow` fields (plus `ctx_max`, `api.ts:868`, which the card currently ignores).

### Layout

```
┌─────────────────────────────────────────────────────────────┐
│ [box] Qwen3.8-27B            [Q4_K_M] [verified ✓] [⧉ hash] │  header: name, quant chip,
│       qwen3-27b-planner  (mono sub-line, only if renamed)   │  integrity chip, hash copy btn
│ [State chip]  [progress bar — download states only]         │  state as a colored chip;
│                                                             │  bar encodes downloaded share
│ File: …/Qwen3.8-27B-IQ3_S.gguf [⧉]   Size: 11 GB            │  Label: Value tiles
│ Parameters: 27B                      Context: 262,144       │
│ Requests (1h): 1,234   Errors: 3   Throughput: 45 tok/s     │  stats tiles + error-share
│ [error-share bar]                                           │  bar — only with a window
│ [Verify checksum] [Delete] [Disable] [Rename]               │  actions (unchanged)
│ ▸ Details                                                   │  collapsed expandable:
│     SHA-256: <full 64-char hash> [⧉]                        │  full hash, verified date,
│     Verified: 2026-10-16                                    │  full path, model id
│ (error / note lines, unchanged, directly under actions)     │
└─────────────────────────────────────────────────────────────┘
```

### What becomes a visual, and what each element encodes

- **Download progress bar** (`meter`, `charts.ts:81`): fill = `partial_bytes / total`, where total is the on-disk size when known, else `catalog_size_bytes`. One bar for the downloading, paused, and partial states. The byte counts appear once, as the labeled value beside the bar ("Downloaded: 12.3 GB of ~49 GB") — and the Size tile stops repeating them in those states (Size shows the final size only when `on_disk`).
- **Error-share bar** (`stackedBar`, `charts.ts:62`): two segments — errors vs requests−errors over the 1h window — shown only when a metrics window exists. It encodes the error proportion; the counts themselves live in the tiles, not in the bar's text.
- **State chip**: the lifecycle state as a single chip, colored per principle 11 (below). No other visual is added — the card has no other multi-number data that a chart would clarify.

### What becomes Label: Value

- Tiles row (extends the existing `.kv` pattern): File (path, ellipsized, full path in tooltip), Size, Parameters (`trim1`-formatted: "Parameters: 27B"), Context (`ctx_max` with thousands separators, only when non-null).
- Stats row, only with a metrics window: "Requests (1h): 1,234", "Errors: 3", "Throughput: 45 tok/s". A missing field omits its tile — never a coerced 0.
- Details expandable: "SHA-256: <full hash>", "Verified: <date>", "File: <full path>", "Model ID: <id>".

### What moves to tooltip / expandable

- "the download finished and the file is being checked against the pinned SHA-256" → tooltip on the Verifying chip.
- "the file is incomplete and cannot be used yet" → tooltip on the Partial chip.
- The 16-char hash fragment line (`:2135`) is deleted; the full hash moves to Details with a copy button, and a copy button also sits beside the header integrity chip.
- The basis/estimate explanations stay as the "(estimate)" qualifier on the value itself (already compliant) — no new prose.

### Status colors (principle 11 applied)

| State | Chip | Color |
|---|---|---|
| Verified (`modelShaBadge`, `:2030`) | "sha256 verified <date>" | Green (`badge-ok`) — unchanged |
| Downloading / Paused / Verifying | state chip | Amber (`badge-warn`) — transitional |
| Partial / download failed | state chip | Light red (`badge-err`) |
| Disabled | "disabled" chip | Gray (plain `.chip`) — was amber at `:2126` |
| Hash not pinned | chip | Gray with warn icon + tooltip — was amber at `:2033` |
| Pinned, not verified | chip | Gray (plain `.chip`) — unchanged, now consistent with the disabled treatment |

### Copy buttons

- Full sha256: beside the header integrity chip and repeated in Details (same value, one affordance per location is fine — the value itself is printed once, in Details).
- File path: beside the File tile.

### Omitted when empty

- No metrics window → no stats row, no placeholder text (fixes `:2136` fallback).
- No hash → no hash line anywhere outside Details, and Details omits the SHA-256 row; the chip alone carries "hash not pinned".
- `params_b` = 0 → no Parameters tile (current behavior at `:2133`, kept). `ctx_max` null → no Context tile.
- Not renamed → no id sub-line (current behavior at `:2129`, kept).
- Not in a download state → no progress bar and no state chip beyond integrity/disabled.

### Formatting fixes

- Parameters through `trim1` (`:2133`): "27B", "30.5B" — never two decimals.
- Context with `toLocaleString`, matching the requests count at `:2136`.

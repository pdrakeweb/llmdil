# Design Audit — Dashboard Service Card

- **Card:** Dashboard service card (Dashboard / Appliance view, Backends group; the first card represents the gateway)
- **Source:** `serviceCard`, `ui/src/views/dashboard.ts:61-121`; primitives in `ui/src/charts.ts`; model in `ui/src/types.ts`
- **Standard:** `docs/design-principles.md`, Part 3 checklist (14 questions; every answer must be "no")
- **Method:** code read only; no code changed.

## Verdict

**Minor gaps.** 6 of 14 checklist questions fail. Four of the six share one root cause — the gateway variant's gauge ring — and the other two are one-line fixes that affect both variants. The card's foundations are sound: it is compact, carries no inline narrative, and handles unmeasured data honestly (em dashes and omitted strips, never fabricated zeros). Nothing here needs a rebuild; the gateway ring needs to earn its slot or lose it.

## Scorecard

| # | Question | Result | Basis |
|---|---|---|---|
| 1 | Compact | Pass | Ring + two metrics + optional sparkline + action row; all summary-level, nothing expandable-worthy held inline. |
| 2 | One section, multiple views | Pass | No data is presented two ways as stacked blocks. (The ring/row duplication is scored under #6.) |
| 3 | Prime real estate | **Fail** | Gateway variant: the largest visual element encodes nothing (see #5); the card's answers sit in small text rows beside it (`dashboard.ts:87-96`). |
| 4 | Narrative in tooltips | Pass | Visible strings are labels, chart keys, and button text only; no prose anywhere in the card. |
| 5 | Real data | **Fail** | Gateway ring never encodes a value by construction (`dashboard.ts:87`); on live data the backend ring's arc never fires either (no live source for `health`, `dashboard.ts:63-67`). |
| 6 | Numbers in one place | **Fail** | Gateway `requests_1h` is shown twice: ring center (`dashboard.ts:70`) and the REQUESTS / 1H row (`dashboard.ts:75`). |
| 7 | Density with order | Pass | Identity → performance summary → trend → actions; summary before detail throughout. |
| 8 | Multi-number data is a chart | **Fail** | Gateway requests + errors are a related pair (their ratio is the informative number) rendered as two text rows while the ring that could encode the ratio sits empty (`dashboard.ts:74-82`). |
| 9 | Label: Value | Pass | Labels are nouns (THROUGHPUT, VRAM HELD, REQUESTS / 1H, ERRORS / 1H); values are labeled pairs. |
| 10 | Copy buttons | Pass | The card renders no URL, key, or address — only a port suffix in the header. The copyable endpoint lives on the Client endpoint card; if an address is ever added here it needs a copy button. |
| 11 | Status colors | **Fail** | Ring color maps `degraded` and `idle` to red, contradicting the status dot on the same card and the semantics in principle 11 (`dashboard.ts:62` vs `dashboard.ts:53-56`). |
| 12 | Unknown is not zero | Pass | Absent metrics render "—" (`dashboard.ts:70-82`), the sparkline strip is omitted when no series exists (`dashboard.ts:97-104`), and the ring falls back to a neutral track (`dashboard.ts:88-92`). |
| 13 | Empty states | Pass | No placeholder prose in the card; absent data is an em-dash affordance (#12) or an omitted element. |
| 14 | Number formatting | **Fail** | Gateway ring-center count uses `String()` with no thousands separator (`dashboard.ts:70`) while the same count in the row uses `toLocaleString()` (`dashboard.ts:75`); the errors count also uses bare `String()` (`dashboard.ts:80`). |

## Findings

### F1 — The gateway ring encodes nothing, and duplicates the number it frames (checklist #3, #5, #6, #8)

`serviceCard` deliberately suppresses the gateway ring's arc: `ringMetric` is hard-set to `undefined` for the gateway (`dashboard.ts:87`), so `gaugeRing` is always called with value 0 and the neutral `var(--line)` color (`dashboard.ts:88-92`). The reasoning in the comment (`dashboard.ts:83-87`) is correct as far as it goes — a request count is not a percentage, and a fake arc would be worse. But the result is a 64px gauge-shaped frame whose only content is the raw count, in the card's prime visual slot, while:

- the same count is repeated in the REQUESTS / 1H row immediately to the right (`dashboard.ts:70` vs `dashboard.ts:75`) — the exact "value in a chart repeated in a text block beside it" that principle 7 prohibits, down to the label being duplicated too ("req/1h" ring sublabel, `dashboard.ts:72`, vs "REQUESTS / 1H");
- the one derived number the ring *could* honestly encode — the success/error share of the last hour's requests — is nowhere on the card; requests and errors sit as two unrelated text rows (`dashboard.ts:74-82`).

The backend variant has the mirror-image problem on live data: the arc is driven by `health` (`dashboard.ts:88`), but per the in-code comment `health` and `vram_held_gb` "still have no live source" (`dashboard.ts:63-67`; `ui/src/types.ts:24-32` likewise marks these fields sample-only). So in production every backend ring is also a permanent neutral track with an em-dash center. The absence handling is honest — this is a design gap, not a data-integrity bug: the card's prime visual element currently answers its question only in sample data.

What does correspond, for the record: when `health` is present, the backend arc length is exactly `health` percent (`dashboard.ts:90-92` into `charts.ts:39-55`), the sparkline plots the actual series (`dashboard.ts:97-104`), and the status dot's color maps the actual status (`dashboard.ts:53-56`).

### F2 — Ring color breaks the status color semantics (checklist #11)

```ts
// dashboard.ts:62
const ringColor = b.status === "ok" ? C.green : b.status === "starting" ? C.amber : C.red;
```

Every status other than `ok`/`starting` — including `degraded` and `idle` (`ui/src/types.ts:11`) — gets a red arc. The status dot in the same card's header maps `degraded` to amber ("warn") and unknown/idle to gray (`dashboard.ts:53-56`), and the Topology view's `statusColor` agrees with the dot (`dashboard.ts:290-291`). A degraded backend therefore shows an amber dot and a red ring on one card, and an idle backend with a health value would show a gray dot and a red ring. Principle 11 reserves light red for stopped/failed and amber for transitional/warn states; the ring is the outlier of the three mappings in this file.

### F3 — Count formatting is inconsistent, including the same number twice on one card (checklist #14)

- `dashboard.ts:70`: gateway ring center is `String(b.requests_1h)` — 12500 renders as "12500".
- `dashboard.ts:75`: the same value in the row is `b.requests_1h.toLocaleString()` — "12,500".
- `dashboard.ts:80`: errors count is `String(b.errors_1h)` — unseparated again.

Principle 14 requires thousands separators for counts. The single-value formatters are compliant (`trim1`, `charts.ts:132-137`; `fmtGb`, `charts.ts:140-142`; health is rendered as an integer percent), so this is confined to the gateway count paths.

### Nit — Sparkline color is keyed on hardcoded sample-data backend ids

`dashboard.ts:101` picks the sparkline color from `b.id === "coder-30b"` / `"coder-7b"`. Color here is series identity, not a value encoding, so this is not a checklist failure — but production code branching on sample ids is fragile decoration; a stable per-card palette assignment (as the placement rows already do, `dashboard.ts:144-159`) or a single accent color would be cleaner.

## Redesign proposal

Scope: `serviceCard` only, plus one shared helper. No payload changes; every value below is already on `BackendInfo`.

1. **Give the gateway ring the error share.** Arc percent = `requests_1h > 0 ? 100 * (1 - errors_1h / requests_1h) : undefined`; center = that share as an integer percent; sublabel "success / 1h". This mirrors the backend card exactly (arc + center = one derived metric; rows = the raw counts) and resolves F1 and #8 in one move: the ring now encodes a real proportion, and no number appears twice.
2. **Keep the two gateway rows as the counts, formatted once, consistently.** REQUESTS / 1H and ERRORS / 1H, both via `toLocaleString()` (fixes F3). The ring center no longer carries the request count, so nothing is duplicated.
3. **Omit the backend ring when it has nothing to encode.** When `health` is undefined (today: all live data), render the card without the ring and let the metric rows take the full width; the ring returns when a live health source exists. An absent visual for absent data follows the same rule the sparkline already uses (`dashboard.ts:97-104`), instead of a permanent empty gauge occupying prime real estate. If a live health source is wired into the metrics poller later, this condition simply stops triggering.
4. **One status→color mapping per file.** Hoist the Topology view's `statusColor` (`dashboard.ts:290-291`) to module scope and use it for both the status dot and the ring arc color: ok → green, degraded/starting → amber, down → red, else gray. Fixes F2 and prevents the three mappings from drifting again.
5. **Format all counts through one helper.** A `fmtCount` beside `trim1`/`fmtGb` in `charts.ts` (integer + `toLocaleString`) used for both gateway rows and any future count readouts.
6. **Sparkline color:** drop the id checks; use the single accent color (gateway) / one stable palette color per card index (backends), matching the placement palette approach.

Result if applied: gateway card = ring (success share, real arc) + two count rows + optional sparkline; backend card = ring only when health exists, throughput + VRAM rows, optional sparkline. All 14 checklist items pass for both variants, and the two variants become structurally identical — one derived gauge metric, two labeled single values, one trend strip.

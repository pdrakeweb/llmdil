# Design Audit — Routing Rules Card

Card: Routing rules card, Gateway view. Source: inline in `gatewayPaint`, `ui/src/views/static.ts:2998–3006`.
Data: `GatewayConfigData.routing_rules: GatewayRule[]` (`ui/src/api.ts:1167–1171`), from `RoutingRule` (`crates/manager-config/src/lib.rs:187–195`).
Verdict: **Redesign needed** — 5 gaps of 14. The card's primary content is prose; the rule's operative data is never rendered.

## What the card renders today

One row per rule (`static.ts:3000–3004`): the rule's `description` as bare 12px text (`:3001`), its `id` as dimmed monospace (`:3003`), and its `source` as a plain chip (`:3004`). At zero rules the card is omitted entirely (`:2998`, `:3006`).

The data model carries a fourth field the card never shows: `litellm_fragment` — the rule's actual LiteLLM config fragment, i.e. what the rule *does* (`crates/manager-config/src/lib.rs:187`, `:192`). It already reaches the client (`get_gateway_config` serializes the whole `GatewayConfig`, `src-tauri/src/main.rs:2679`) but is untyped in the UI's `GatewayRule` (`ui/src/api.ts:1167–1171`: id, description, source only) and rendered nowhere.

## Scorecard

Every question must answer "no" to pass.

| # | Question | Result | Evidence |
|---|---|---|---|
| 1 | Compact — anything that belongs behind a toggle/expandable? | **FAIL** | Flat unbounded list: N rules = N inline rows, no collapse (`static.ts:2999–3005`). Per-rule detail is inline instead of on demand. |
| 2 | One section, multiple views — same data two ways, stacked? | Pass | Single representation (one list). |
| 3 | Prime real estate — top element other than the answering visual/value? | **FAIL** | First element under the title is the first rule's prose description (`static.ts:3001`). No visual, no summary value; the rules' effects are not rendered at all. |
| 4 | Narrative in tooltips — explanation visible inline? | **FAIL** | `r.description` is by design "a plain-language routing rule" (`crates/manager-config/src/lib.rs:187`) and is rendered inline as the row's primary content (`static.ts:3001`). |
| 5 | Real data — any visual mark not corresponding to a value? | Pass | No visual encodings exist to be false. (Their absence is scored at Q3/Q8.) |
| 6 | Numbers in one place — any number shown twice in the card? | Pass | The card renders no numbers. The rule count lives once, in the gateway summary card (`static.ts:2875`); the redesign must not restate it here. |
| 7 | Density with order — grouping/summary order wrong? | Pass | Rows group by rule, the question the card answers. Within-row ordering (prose before identifier) is treated under Q9. |
| 8 | Multi-number data is a chart — related numbers as text? | **FAIL** | The rule set's composition (count by source: "wizard default" vs "user", `crates/manager-config/src/lib.rs:193–194`) is multi-value categorical data presented only as repeated per-row text chips (`static.ts:3004`); no aggregate visual exists. |
| 9 | Label: Value — any visible string a sentence, any label a sentence? | **FAIL** | The description is a bare sentence with no label (`static.ts:3001`); the `id` (`:3003`) and `source` (`:3004`) are values with no labels. Nothing in the card is in Label: Value form. |
| 10 | Copy buttons — any URL/key/address without one? | Pass | The card renders no URL, key, or address. (`r.id` is a config identifier, outside the enumerated classes.) |
| 11 | Status colors — semantics broken, or a chip stating the default? | Pass | The `source` chip is plain and uncolored (`static.ts:3004`); source is provenance, not status, and both values ("wizard default", "user") are informative. |
| 12 | Unknown is not zero — unmeasured rendered as 0? | Pass | No measured data in the card; rule state is config, always known. |
| 13 | Empty states — placeholder prose when no data? | Pass | Exemplary: at 0 rules the card renders `""` (`static.ts:2998`, `:3006`) — the block is omitted, per principle 13. |
| 14 | Number formatting — decimals/trailing `.0`/magnitude violations? | Pass | No numbers rendered (vacuous). |

Gap count: **5** (Q1, Q3, Q4, Q8, Q9). Q3/Q4/Q9 are one root cause — the narrative *is* the content model — counted as three checklist failures because the checklist asks them separately.

## Redesign

Keep: the zero-rules omission (Q13 behavior is the model for the rest of the view), the plain provenance chip, and the rule count's single home in the gateway summary card.

1. **Prime visual — source composition bar.** Directly under the "Routing rules" title, when there are ≥2 rules: one segmented bar (the existing segmented-bar primitive in `ui/src/charts.ts`) whose segments are sized by rule count per source — segment 1 wizard-default rules, segment 2 user rules — with the source names as the chart key beneath it. This is the card's answering visual (Q3) and turns the repeated chips' aggregate into a chart (Q8). Source is provenance, so segments use neutral/gray tones, never status colors (Q11). With <2 rules or a single source, the bar encodes nothing the rows don't; omit it.
2. **Rows become Label: Value with the narrative demoted.** Each rule row: primary line "**Rule:** `id`" (the id is the rule's identifier — label + value, mono value as today); the `source` chip stays at the right. The `description` moves verbatim into an info-tip on the row (Q4, Q9). Nothing else is visible inline.
3. **The operative data gets an expandable.** Each row gains a collapsed expandable, "Fragment", revealing the rule's `litellm_fragment` as a mono block — the actual LiteLLM fragment the rule contributes to the generated config. This puts the detail that extends the page behind a compact control (Q1) and finally renders the one field that says what the rule does. Prerequisite is type-only: add `litellm_fragment` to `GatewayRule` (`ui/src/api.ts:1167–1171`); the value already arrives in the `get_gateway_config` payload (`src-tauri/src/main.rs:2679`), so no backend change is needed.
4. **Ordering.** Title → composition bar (when present) → rule rows in config order (config order is evaluation order for routing rules, so row order itself encodes precedence — keep it, and let the Fragment expandable carry any per-rule explanation).

Resulting closed card: title, one bar with a two-word key, and one labeled line per rule. Everything else — the plain-language description, the fragment — is one hover or one click away, and at 0 rules the card still does not exist.

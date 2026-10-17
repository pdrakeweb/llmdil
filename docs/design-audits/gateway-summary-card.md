# Design Audit — Gateway Summary Card

Card: Gateway summary card (Gateway view header block: status title, Port, Groups, Routing rules, Routing test traces, gateway metrics block)
Source: inline in `gatewayPaint`, `ui/src/views/static.ts` (card markup at lines 2880–2886; metrics block at lines 2870–2879)
Audited against: `docs/design-principles.md` — 14 principles, Part 3 checklist
Date: 2026-10-16
Verdict: **Minor gaps** — 5 of 14 checklist items fail. The card's skeleton (status title, tile grid, narrative in tooltips) is sound; the failures are localized to the metrics block, two value formats, one label, and count formatting.
Code changes: none (audit only).

## Scorecard

| # | Checklist item | Result | Evidence |
|---|---|---|---|
| 1 | Compact | Pass | One title row, one 4-tile grid, one conditional stats block (`static.ts:2880–2886`). Nothing in the card belongs behind a tab or expandable. |
| 2 | One section, multiple views | Pass | Each datum appears once, in one representation: identity counts as tiles (`static.ts:2882`), traffic totals as tiles (`static.ts:2871–2878`). No stacked dual representation. |
| 3 | Prime real estate | Pass | The top element is the answer: status dot + "Gateway — LiteLLM · up/down" (`static.ts:2880`), value tiles immediately below. |
| 4 | Narrative in tooltips | Pass | Both explanations are behind info-tips: the routing-traces explanation (`static.ts:2882`, third tuple element) and the metrics-window explanation (`static.ts:2879`, info-tip `title`). No prose sentence renders inline. |
| 5 | Real data | Pass | The only visual mark is the status dot, bound directly to `status.up` (`static.ts:2880`; `dot()` mapping at `static.ts:38–40`). Tiles render literal payload values. See the observation on the Groups count below. |
| 6 | Numbers in one place | Pass | Within the card, every number appears once. (The port recurs in the Client endpoint card — a different card, out of scope. The up state is encoded twice in the title, dot + the word "up"; that is text/color redundancy for legibility, not a duplicated number.) |
| 7 | Density with order | Pass | One question — gateway status and traffic. Identity counts first, then the 1 h traffic block (`static.ts:2870–2886`); per-group detail lives in the group cards below. |
| 8 | Multi-number data is a chart | **Fail** | Finding F1 |
| 9 | Label: Value | **Fail** | Finding F2 |
| 10 | Copy buttons | Pass | Not applicable: the card holds no URL, key, or address — the port is a bare number. Copy buttons for URLs and the master key live on the Client endpoint card. |
| 11 | Status colors | Pass | `dot(status.up ? "ok" : "err")` (`static.ts:2880`) with `dot()` mapping ok → green, err → red (`static.ts:38–40`) — green = running, light red = down, per principle 11. No chips stating a default state. |
| 12 | Unknown is not zero | **Fail** | Finding F3 |
| 13 | Empty states | **Fail** | Finding F4 |
| 14 | Number formatting | **Fail** | Finding F5 |

## Findings

### F1 — The 1 h traffic block is four related numbers rendered as text (Principle 8)

`static.ts:2871–2878` renders Requests, Errors, Throughput, and Hottest backend as four `kv` text tiles. Two of these are intrinsically chart-shaped: requests per backend is a distribution (the "Hottest backend" tile is a workaround for not showing it), and errors against requests is a share. No chart primitive from `ui/src/charts.ts` is used anywhere in the card. The identity counts in the header grid (Port, Groups, Routing rules, Routing test traces) are single values and are correctly tiles — the failure is specific to the traffic block.

### F2 — Three values/labels are not clean "Label: Value" (Principle 9)

All at `static.ts:2882` and `static.ts:2871–2875`:

- Routing test traces value is `` `${status.routing_log_len} recorded` `` — a word inside the value. The value should be the bare count; the "recorded" sense is already carried by the label and its tooltip.
- Port value is `` `:${port}` `` — the value carries the separator glyph, rendering as "Port :4000" once the kv layout supplies its own separation. The value should be `4000`.
- The stats label "Requests (1h, all backends)" (`static.ts:2872`) embeds a scope clause in the label — and the clause is not always true (see F3). "Hottest backend" shows `id (N req)` (`static.ts:2875`), a second value parenthesized inside one tile.

### F3 — A partial metrics sum is labeled as a complete total (Principle 12)

`static.ts:2870` filters to backends that have a metrics window (`requests_1h !== undefined`), and the totals at `static.ts:2872–2874` sum only that subset — while the lead tile is labeled "Requests (1h, all backends)". When some backends have no window, their traffic contributes 0 to a total whose label claims full coverage: unmeasured data silently counted as zero. (The fully-empty case is handled correctly by the `static.ts:2879` branch — it avoids rendering zeros; see F4 for its separate problem.)

### F4 — No metrics window still renders a placeholder line (Principle 13)

`static.ts:2879`: when no backend has a metrics window, the card renders a visible line — "Gateway metrics: —" with an info-tip. The explanation prose correctly moved into the tooltip, but principle 13 is explicit: when a data block has no data, the block is omitted rather than showing placeholder prose — and the em-dash line is a placeholder block. This is the same ruling already applied to the backend card's stats row (no window → no row). The metrics block should render nothing when `withWin` is empty.

### F5 — Counts render without thousands separators (Principle 14)

Counts are interpolated with `String(...)` (`static.ts:2872`, `2873`, `2882`) — a 1 h request total of 1234 renders as "1234". Principle 14 requires thousands separators where the value is a count. Throughput is compliant via `trim1` (`ui/src/charts.ts:132–137`: ≤1 decimal, trailing `.0` dropped, whole at ≥100), but `trim1` does no grouping either, so the fix belongs in a count formatter used by both.

## Observation (not scored)

- **Groups count semantics.** The Groups tile renders `status.groups` (`static.ts:2882`), which is the configured-group count from the status payload. Since v0.1.32, the generated gateway config contains only enabled groups, so "Groups: 5" can describe a gateway actually running 3. That is a payload-semantics question (already open with the product owner: should `gateway_status` count enabled groups only?), not a rendering defect — the tile faithfully shows the value it is given. Recorded here because principle 5 audits will keep tripping over it until the payload question is settled.
- The `backendsError` banner (`static.ts:2866–2868`) renders inline prose above the card. It is an error state for the whole view rather than card content, and it carries information (exclusion states may be incomplete) that has no other home; left out of this audit's scope.

## Proposed redesign

Keep the card's skeleton — it passes layout, order, tooltip, and color checks. Change only the metrics block, the value formats, and count formatting.

**Layout (top to bottom):**

1. Title row — unchanged: status dot + "Gateway — LiteLLM · up/down".
2. Identity tile row — unchanged grid, values fixed per F2/F5: `Port: 4000`, `Groups: 5`, `Routing rules: 0`, `Routing test traces: 5`.
3. Traffic block — **rendered only when at least one backend has a metrics window** (F4). When it renders:
   - A per-backend requests bar (stacked bar from `charts.ts`): one segment per reporting backend, segment width = that backend's `requests_1h`, key = backend id with its count. This replaces both the "Requests (1h, all backends)" tile (the total is the bar's whole, labeled once as the row's value: `Requests (1h): 1,234`) and the "Hottest backend" tile (the largest segment is visible as such; its name and count are the segment key — no separate tile, so no number is shown twice, principle 7).
   - Errors as a share visual: a thin proportion bar under the requests bar, filled `errors_1h / requests_1h`, with the row value `Errors (1h): 37`. The fill encodes the error rate; the count lives in the label value only.
   - Throughput stays a single-value tile (`Throughput: 812 tok/s`) — a single current rate is a value, not multi-number data; no series is available to this card, so no sparkline is invented (principle 6).

**Data honesty (F3):** when only some backends report, the traffic block's heading value carries the coverage: `Requests (1h): 1,234` with an info-tip "3 of 5 backends reporting — totals cover reporting backends only", and the bar contains only reporting backends' segments. The phrase "all backends" is used only when every backend reports.

**What moves where:**

| Element | Treatment |
|---|---|
| Metrics-window explanation ("No gateway metrics window yet — …") | Deleted with the placeholder line (F4). No element remains to anchor it; per principle 13 the absence is self-evident. |
| Routing-traces explanation | Stays in the label's info-tip (already compliant). |
| "Hottest backend" tile | Removed; encoded by the requests bar's largest segment and its key. |
| Requests/Errors totals | Bar row values (Label: Value), counts formatted with thousands separators. |
| Throughput tile | Unchanged position, value formatted via `trim1`. |
| Port, Groups, Routing rules tiles | Unchanged except Port value drops the leading colon. |

**Formatting:** add a count formatter alongside `trim1` in `ui/src/charts.ts` (integer, thousands separators) and use it for Groups, Routing rules, Routing test traces, Requests, and Errors values in this card.

**Omitted when empty:** the entire traffic block (no line, no dash, no tooltip) when no backend has a metrics window.

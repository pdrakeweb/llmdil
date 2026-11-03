# Design Audit — Throughput Chart Card

Card: Throughput chart card (Dashboard, Appliance view; renders only when a series exists)
Source: inline in `applianceView`, `ui/src/views/dashboard.ts:204-210`; chart primitive `multiLine`, `ui/src/charts.ts:90-117`
Audited against: `docs/design-principles.md` Parts 1 and 3
Verdict: **Redesign needed** — 4 gaps (3 under checklist Q5, 1 under Q12)

## Scorecard

| # | Question (must be "no") | Result | Evidence |
|---|---|---|---|
| 1 | Compact — anything that belongs behind a toggle/expandable? | Pass | The card body is a single chart; nothing else to demote (`dashboard.ts:207-209`). |
| 2 | One section, multiple views — same data two ways in the card? | Pass | One representation (multi-series line) per card (`dashboard.ts:209`). |
| 3 | Prime real estate — top element anything other than the answer visual? | Pass (note) | Title (`dashboard.ts:207`) and legend (`dashboard.ts:208`) precede the chart, but both are chart furniture — a label and the chart key, which Principle 5 permits as visible text. The chart is the card's entire body. View-level position (second row, below GPUs/Backends groups) follows Pete's directed dashboard order, not this card's choice. |
| 4 | Narrative inline? | Pass | No prose. The title's "· sample data" (`dashboard.ts:207`) is a provenance label, not methodology, and it is load-bearing honesty (see Q5). |
| 5 | Real data — every mark corresponds to an actual value? | **FAIL** | See Findings 1–3. The card has no live data source at all; the only values it ever draws are fabricated. Geometry is faithful but unreadable (no scale), and the area fill overshoots its baseline. |
| 6 | Numbers in one place — any number shown twice in the card? | Pass (note) | The card renders no numbers at all: legend is name + swatch only (`dashboard.ts:208`). Cross-card note, not scored here: the same throughput story also appears as an instantaneous tok/s value and a per-backend sparkline in the service cards (`dashboard.ts:75-76`, `dashboard.ts:97-101`); view-level duplication is the service card audit's problem to resolve. |
| 7 | Density with order — grouped by its question, summary first? | Pass | Title → key → chart is the natural reading order for a history chart. |
| 8 | Multi-number data as text instead of a chart? | Pass | The multi-number data *is* the chart (`multiLine`, `charts.ts:90`). |
| 9 | Label: Value — any sentence, any sentence-label? | Pass | Title is a chart title, legend entries are chart keys (Principles doc, Principle 5 carve-out). No sentences. |
| 10 | Copy buttons — any URL/key/address missing one? | Pass (n/a) | The card contains no URL, key, or address. |
| 11 | Status colors — any semantics break, any default-state chip? | Pass (note) | No status chips or state colors. Series colors are identity colors from the shared palette `C` (`dashboard.ts:53`, assigned in `mock.ts:79-82`) and match the service-card sparkline mapping for the same backends (planner blue, coder-30b purple, coder-7b teal — `dashboard.ts:99`). Any future live wiring must keep that identity mapping rather than inventing a second one. |
| 12 | Unknown is not zero — unmeasured rendered as 0 or as measured? | **FAIL** (minor) | See Finding 4. Absence of one backend's series is invisible; there is no "not measured" affordance. |
| 13 | Empty states — placeholder prose when no data? | Pass | Exemplary: with no series the card is omitted entirely (`dashboard.ts:206-210` yields `""`). |
| 14 | Number formatting — >1 decimal, trailing `.0`, decimals ≥100? | Pass (vacuous) | The card prints no numbers, so nothing can violate the rule. This is the flip side of Finding 2: there are no axis labels to format. |

## Findings

### Finding 1 (Q5) — The card has no real data source; it only ever draws fabricated values

- The live dashboard model hardcodes an empty series: `throughputSeries: []` (`ui/src/api.ts:379`). The card therefore never renders in production.
- The only producer of a series is the mock model (`ui/src/mock.ts:78-83`), whose values come from `walk()` — a deterministic pseudo-random walk (`ui/src/charts.ts:120-133`), not from any measurement.
- The code knows this: the comment at `dashboard.ts:204-205` states "A series exists only in sample data (the live path reports none)", and the title appends "· sample data" when not live (`dashboard.ts:207`).

The disclosure keeps this honest — sample marks are labeled as sample — but against Principle 6 ("every visual element encodes real data; a chart whose marks don't correspond to the values is worse than no chart"), the card as built is a placeholder for a chart, not a chart: in the only mode where it appears, every mark corresponds to an invented number, and in the mode where real numbers exist, it cannot appear. This is the card's defining defect and the reason for the redesign verdict.

### Finding 2 (Q5) — No axis or scale: the faithful geometry cannot be read back

`multiLine` (`charts.ts:90-117`) maps values linearly and exactly — vertex *i* of each polyline is value *i*, scaled by `(v − min) / span` (`charts.ts:104-106`) — but it emits no axis labels, no min/max annotations, no time labels, and no units. A reader can see shape and relative ordering and nothing else: no throughput value, range, or time can be recovered from the chart. A chart whose correspondence to values exists but is unverifiable by its reader fails the intent of Principle 6 even though the mapping math is correct.

### Finding 3 (Q5) — Area fill overshoots its baseline; rendering stretches non-uniformly

- Each series' area polygon closes along `y = h` (`charts.ts:109-110`), but the value scale maps the minimum (0 for throughput, since `min = Math.min(...all, 0)`, `charts.ts:97`) to `y = h − 4` (`charts.ts:105`). The fill therefore extends 4 px below the zero line — a small region of ink that corresponds to no data, on every series.
- The SVG uses `preserveAspectRatio="none"` with `width:100%` (`charts.ts:115`), so the 560×220 viewBox stretches non-uniformly to whatever width the grid cell takes; identical data draws steeper or shallower slopes depending on layout width. Values are not falsified, but slope — the chart's main visual claim about rate of change — is layout-dependent.

### Finding 4 (Q12, minor) — A backend with no series disappears silently

The card keys entirely off the series array (`dashboard.ts:206-209`): if a future live source supplies series for some backends and not others, the missing backend simply has no line and no legend entry. Principle 12 requires an explicit "not measured" affordance for unmeasured data, not invisibility. (Today this is latent — the array is all-or-nothing — but any real wiring, per Finding 1's fix, will hit it immediately.)

## Redesign proposal

1. **Give the chart a real series.** The app already polls per-backend throughput (`throughput_toks` per backend, sourced from the gateway metrics poller) on the dashboard's 5 s poll. Retain the last N samples per backend (with timestamps) in the dashboard model assembly in `api.ts` and emit them as `throughputSeries`. Label the window from the actual retained span (e.g. "· last 15 min", growing toward the "1h" the title already claims) — the title must describe the retained window, not an aspiration. Until at least a few samples exist, keep the current Q13 behavior: no card.
2. **Add a scale.** In `multiLine` (or a throughput-specific wrapper): y-axis labels for max and mid using `trim1` with a single "tok/s" unit label; x-axis labels for window start/end times. This also gives Q14 something to bind to.
3. **Fix the fill.** Close area polygons at the mapped zero line (`h − 4` under the current scale), not the SVG's bottom edge.
4. **Stop the stretch.** Drop `preserveAspectRatio="none"` for this chart (fixed-aspect rendering, or `vector-effect` with a viewBox matched to the cell), so slope is a property of the data, not the layout.
5. **Represent absence.** Extend the series model so a backend can be present with no samples; render it in the legend as "name · not measured" with a hollow swatch and no line (Principle 12), instead of omitting it.
6. **Make the legend the single home of the current value.** Add each series' latest value to its legend entry ("coder · 22.6 tok/s", `trim1`-formatted). That keeps numbers in one place *within this card* and gives the chart a readable headline value; reconciling the duplicate instantaneous tok/s in the service cards is left to that card's audit, flagged here as the known cross-card overlap.
7. **Keep what already complies.** The empty-state omission (Q13), the provenance label in sample mode, and the identity-color mapping shared with the service cards are correct and should survive the redesign unchanged.

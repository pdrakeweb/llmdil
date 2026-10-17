# Design Audit — Recent Activity Card

**Where it renders:** Dashboard (Appliance view), bottom of the view; only when events exist.
**Source:** inline in `applianceView`, `ui/src/views/dashboard.ts:211–214` (card markup), placed at `dashboard.ts:228`. Row styles: `ui/src/styles.css:332–334`; status dots: `styles.css:149–153`. Data type: `MapEvent`, `ui/src/types.ts:100–104`.

**Verdict: redesign needed.** 6 of 14 checklist questions fail (Q1, Q4, Q7, Q8, Q9, Q11). The failures share one root cause: the event record is `{ time, level, text }` — a timestamp, a level, and a free-text sentence — so the renderer *cannot* present the data as anything but prose rows. The fix starts at the schema, not the markup.

## Precondition finding (read first)

The card has no live data source. The live dashboard model hardcodes `events: []` (`ui/src/api.ts:383`), so in the installed app the card never renders at all. The only producer is the sample model (`ui/src/mock.ts:87–91`), meaning the card appears only in the not-under-Tauri sample mode. The audit below scores the card as designed (against its sample data, the only data it has ever rendered); the redesign in the second half defines the structured event schema a live producer would have to supply. The app already emits suitable raw material elsewhere (`BACKEND_STATUS`, `DOWNLOAD_PROGRESS`, `WIZARD_PROGRESS` event streams in `src-tauri/src/main.rs`) — wiring that producer is separate work from this redesign, but until it exists the redesigned card, like the current one, is sample-only.

## Scores

| # | Question (must be "no") | Result | Evidence |
|---|---|---|---|
| 1 | Compact — content that belongs behind a toggle/expandable? | **FAIL** (minor) | Renderer maps the entire array with no cap and no expandable: `m.events.map(...)`, `dashboard.ts:213`. Page growth is linear in event count; an activity history is precisely the content principle 1 puts behind a compact control. Masked today only because the live array is always empty (`api.ts:383`) and the sample has 5 entries (`mock.ts:87–91`). |
| 2 | One section, multiple views — same data two ways, stacked? | PASS | Single list, one representation (`dashboard.ts:212–214`). Overlap with the Logs view is cross-view, not stacked blocks inside this card. |
| 3 | Prime real estate — top element not the answer? | PASS | Card sits last in the view (`dashboard.ts:228`), and within the card the first row is the newest event (sample order is newest-first, `mock.ts:87–91`). Note: newest-first is a producer contract — the renderer does not sort (`dashboard.ts:213`), so the guarantee is only as strong as the producer. |
| 4 | Narrative in tooltips — explanation visible inline? | **FAIL** | Every entry's content is a prose string rendered inline: `<span>${esc(e.text)}</span>`, `dashboard.ts:213`. The entries are the card's payload rather than methodology, but principle 5 admits only numbers, labels, and chart keys as visible text — a card whose entire body is narrative sentences is the pattern the principle excludes. Sample forms: `mock.ts:87–91`. |
| 5 | Real data — any mark not corresponding to a value? | PASS | The only marks are the level dot (color = `e.level`, mapping at `dashboard.ts:213`, palette at `styles.css:150–153`) and the timestamp (`e.time`). No bars, fills, or gauges assert anything unmeasured. Caveat carried into the redesign: uniform row spacing does not encode elapsed time (sample gaps range from 102 s to 31 min at identical 8px spacing, `styles.css:332`) — spacing is a layout convention here, not a mark, so this is not a Q5 failure, but any timeline treatment must not let spacing pretend to be to-scale (principle 6). |
| 6 | Numbers in one place — any number shown twice? | PASS | Each event's time and figures appear once, in its row (`dashboard.ts:213`). |
| 7 | Density with order — grouped by question; summary before detail? | **FAIL** (minor) | Grouping by recency is right, but the card is flat and all-or-nothing: no summary layer (e.g. how many errors in the window), no detail on demand, no cap (see Q1). The card is also appended in an anonymous wrapper (`dashboard.ts:228`) outside the `grid2` rhythm its sibling cards use, so it reads as an afterthought rather than a grouped section. |
| 8 | Multi-number data is a chart — related numbers as text? | **FAIL** | Entries carry sets of related numbers as prose: "reconnect **3** · backoff **30s**" (count + duration), "download resumed … **62%**" (progress), "**1,204** requests done" (count) — `mock.ts:87–90`. The event sequence itself is temporal data rendered as plain text rows; the only encoded channels are dot color and position in the list. |
| 9 | Label: Value — any visible string a sentence? | **FAIL** | Entry bodies are sentence fragments, not "Label: Value" and not chart keys: "coder-7b queue normalized", "health sweep · all responding" (`mock.ts:88,91`). The schema makes this unavoidable — `MapEvent` has no actor, kind, or numeric fields, only `text` (`types.ts:100–104`). The timestamp column is the one compliant element: a fixed-width mono column (`styles.css:334`) functioning as a chart key. |
| 10 | Copy buttons — any URL/key/address lacking one? | PASS | No URL, key, or address renders in the card (`dashboard.ts:213` renders time + text only). Vacuously compliant. |
| 11 | Status colors — semantics broken, or a chip stating the default? | **FAIL** (minor) | Two deviations. (a) `warn` maps to amber (`dashboard.ts:213`; `.dot.warn`, `styles.css:151`), which principle 11 reserves for transitional states (starting/stopping); a warning event (e.g. a reconnect in backoff, `mock.ts:87`) is not a transition — the color is borrowed from the app's shared status-dot vocabulary, not from the principle's definition. (b) Level — the row's most decision-relevant datum — is conveyed by color alone: no level label appears anywhere in the row (`dashboard.ts:213`), so warn vs error is indistinguishable to anyone who cannot separate the dot colors, and the level never exists as a readable value (principle 9). The ok/error/idle mappings do conform (green/red/gray, `styles.css:150,152,153`). |
| 12 | Unknown is not zero — unmeasured rendered as 0 or as measured? | PASS | No numeric rendering in the card can fake a measurement; absence omits the card (see Q13). |
| 13 | Empty states — placeholder prose when there is no data? | PASS — exemplary | `m.events.length ? <card> : ""` (`dashboard.ts:211–214`), and the placement wrapper is conditional too (`dashboard.ts:228`). No events, no card, no placeholder text. This is the behavior principle 13 prescribes. |
| 14 | Number formatting — >1 decimal, trailing `.0`, decimals ≥100? | PASS | The renderer prints producer strings verbatim (`dashboard.ts:213`); the sample data complies — thousands separator in "1,204" (`mock.ts:90`), integral "62%" (`mock.ts:89`). Contract risk only: nothing stops a future producer emitting "62.0%" or "1204 requests"; the redesigned schema (below) makes figures typed numbers formatted by the renderer, closing this. |

## Redesign proposal

### Start with the schema

Replace the free-text record with a structured event. The renderer's failures are the type's failures (`types.ts:100–104`):

```
{ ts,            // epoch ms; renderer formats HH:MM:SS
  actor,         // short label: backend id or service ("coder", "gateway")
  kind,          // short label: "started" | "stopped" | "failed" | "reconnect" | ...
  level,         // "ok" | "warn" | "error" | "info"
  fields,        // ordered Label: Value pairs, typed numbers: [["Attempt", 3], ["Backoff", "30 s"]]
  detail? }      // the rare true explanation — rendered only in a tooltip/expandable
```

### Rows or timeline? Label: Value rows — decided

A timeline visual was considered and rejected, on the principles' own terms. What a timeline would encode is order, level, and relative recency over a time axis. Event times are bursty and irregular (sample gaps: 102 s vs 31 min in five entries, `mock.ts:87–91`). A to-scale time axis would be mostly empty space (violates principle 1, compact) and stretch the card vertically for no information; a uniform-spaced "timeline" spine would use position — a visual channel — to assert equal intervals that are false (violates principle 6, every visual element encodes real data). The three things worth encoding are carried without an axis: order by newest-first rows, level by the dot, exact recency by the timestamp column. So the redesign keeps the vertical list form and fixes its *content*: each row becomes

`● 06:13:52  mcp-gdrive · Reconnect — Attempt: 3 · Backoff: 30 s`

i.e. dot (level) · mono timestamp (fixed column, as now, `styles.css:334`) · actor label · kind label · the event's numbers as inline Label: Value pairs, formatted by the renderer from typed values (principles 9, 14). Nothing in the row is a sentence.

### Layout

- Card stays at the foot of the Appliance view (its position is correct — Q3), but joins the view's card structure instead of the anonymous margin wrapper (`dashboard.ts:228`).
- Header row: "Recent activity" title; at the right, a level-composition segmented bar for the shown window (counts by level, colored per principle 11 — this is the card's one chart, and it answers "did anything go wrong?" before any row is read, principles 3 and 8) plus "Events: N" as Label: Value.
- Body: the structured rows above, latest 5.
- Footer: a "Show all (N)" expandable, collapsed by default, revealing the remaining rows inside the card — page length bounded regardless of producer volume (fixes Q1/Q7).

### What moves to tooltip / expandable

- `detail` (why a reconnect backed off, a failure's reason string): the row's info-tip, and into the expandable in full — never inline (fixes Q4).
- Full history beyond 5: the "Show all" expandable (fixes Q1).
- Level for warn/error rows: a small text tag ("Warning" / "Error") beside the dot, so level is a readable value, not color-only (fixes Q11b). Ok/info rows get no tag — principle 11's rule that a chip exists only for non-default information, the same logic as the "adopted" chip. The amber question (Q11a) is then moot for reading the row, and the dot keeps the app's shared palette for consistency.

### Empty-state behavior (principle 13)

Unchanged — it is already the model behavior: no events, no card, no placeholder prose (`dashboard.ts:211–214`). Two adjacent states the redesign must also keep honest: (a) events exist but none in the current window — same rule, omit; (b) the sample/live distinction — until a live producer is wired (`api.ts:383`), the card remains sample-only and the existing global "showing sample data" banner is what keeps that honest; the card itself gains no "sample" prose.

### What this does not fix by itself

The producer. A live Recent Activity card needs the app to record structured events (status transitions, download and wizard milestones it already emits as streams). That is backend/payload work beyond this card's markup; the schema above is the contract it should be built against.

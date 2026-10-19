# Design Audit — Logs Filter Card

Card: Logs filter card (Logs view)
Source: inline in `renderLogs` / painted by `wireLogs`, `ui/src/views/static.ts`
Reference: `docs/design-principles.md` (14 principles, Part 3 checklist)
Date: 2026-10-16
Post-v0.1.40 note (2026-10-18): the checklist in `docs/design-principles.md` now runs to 16 questions (rules 15–16 added 2026-10-17/18), and the Logs view has since gained scope deep links — `#/logs?scope=<id>&follow=1`, used by the go-to-logs buttons on backend, gateway, MCP, and Optimize surfaces, with scopes including `optimize` and per-MCP-server scopes (`wireLogs`, `ui/src/views/static.ts`). Line numbers below are the 2026-10-16 snapshot.
Verdict: **Minor gaps** — the control set is sound; the failures concentrate in two always-visible prose strings, a split pagination cluster, and one unformatted count. Targeted fixes, no structural redesign.

## Scorecard

| # | Checklist question | Result |
|---|---|---|
| 1 | Compact | **Fail (minor)** |
| 2 | One section, multiple views | Pass |
| 3 | Prime real estate | Pass |
| 4 | Narrative in tooltips | **Fail** |
| 5 | Real data | Pass |
| 6 | Numbers in one place | Pass |
| 7 | Density with order | **Fail (minor)** |
| 8 | Multi-number data is a chart | **Fail (minor)** |
| 9 | Label: Value | **Fail** |
| 10 | Copy buttons | Pass (no URL/key/address in this card) |
| 11 | Status colors | Pass |
| 12 | Unknown is not zero | Pass |
| 13 | Empty states render nothing | **Fail (minor)** |
| 14 | Number formatting | **Fail (minor)** |

7 of 14 questions fail, but they trace to four elements: the diagnostics-bundle warning line, the retention sentence painted into `#logMeta`, the pagination cluster split across two containers, and the raw total in the page indicator.

## Failures, with evidence

### Q4 / Q9 — Inline narrative: bundle warning (also drives Q1)
`static.ts:2481`, inside the card markup:

```html
<div style="font-size:11px;color:var(--text3);margin-top:8px">The bundle contains raw, unredacted process logs. Review it before sharing.</div>
```

A full explanatory sentence rendered permanently inside the card, on every paint, under the Export button it describes. It violates principle 5 (narrative belongs in a tooltip) and principle 9 (visible strings are Label: Value or chart keys, never sentences), and it is the one element that always extends the card's height for non-data content (Q1). The card already demonstrates the correct pattern one row up: the Level label carries its methodology as a `title` tooltip (`static.ts:2447`, "Level matches line text — supervised processes do not emit structured levels").

### Q4 / Q9 / Q13 — Inline narrative: retention sentence painted into `#logMeta`
`static.ts:2483` (the div) and `static.ts:2740–2742` (painted on every `load()`):

```
Backend logs are kept in memory for this session only — up to 10,000 lines per process; one previous run per process is kept. The gateway log is file-backed (under the data directory's logs folder) and survives restarts.
```

Three methodology sentences rendered as a permanent prose block directly under the card, regardless of whether there is anything to annotate. Same violations as above (principles 5 and 9), plus principle 13: it is a block that renders annotation prose unconditionally rather than being omitted or demoted to an affordance. The same pattern repeats in the results body painted by the same code — the empty state at `static.ts:2750` ("No log lines yet for this scope. Lines appear here once a supervised process writes output.") embeds the explanation inline instead of behind a tooltip.

### Q7 — The pagination question is split across two containers
Page size (`Limit`, `static.ts:2469`) sits inside the filter card; the rest of pagination — Prev/Next buttons and the page indicator (`static.ts:2485` onward, in a separate flex row below `#logMeta`) — sits outside it. One question ("which slice of the result set am I viewing") is answered in two places. Additionally, the Export diagnostics bundle button (`static.ts:2478`) is an app-level diagnostics action grouped into the filter row; it answers a different question than every other control in the row and stretches the row's wrap.

### Q8 — Page position and total rendered as prose only
The card's only multi-number output is the page indicator, painted at `static.ts:2737`:

```
Page ${page} of ${pages} · ${r.data.total} matching records
```

Position within the result set (page / pages / total) is exactly the kind of related-number set principle 8 wants encoded visually; a thin position bar whose fill is `page / pages` encodes it at a glance and satisfies principle 6 (the fill corresponds to a real value).

### Q14 — Matching total has no thousands separator
Same line, `static.ts:2737`: `${r.data.total}` is interpolated raw. Principle 14 requires thousands separators where the value is a count. Log totals routinely exceed 1,000 (the retention note itself cites 10,000-line rings), so the indicator renders e.g. "10000 matching records" unseparated. (The value is at least a whole number — no decimal violation.)

## Passes, briefly

- **Q2** — Scope, Level, Search, From/To, Sort are distinct filter facets, not the same data presented twice.
- **Q3** — The card's top element is the Scope control; for a filter card the controls are the mechanism that answers its question, and the log stream follows immediately below.
- **Q5** — The card contains no bars, gauges, or color encodings that could misrepresent a value.
- **Q6** — The matching total appears once (page indicator); the page-size select value is a control state, not a restated figure.
- **Q10** — No URL, key, or address appears in this card; nothing to copy.
- **Q11** — No state chips in the card; transient messages render errors in the error color (`msg()` in `wireLogs`), consistent with the semantics.
- **Q12** — "0 matching records" is a measured zero returned by a real query, not an unmeasured value masquerading as 0; before the first load the page indicator is simply empty (absent, `static.ts` pagination span starts blank).

## Proposed redesign

Two-row card; the pagination cluster reunified as the card's footer strip; all prose demoted to tooltips with text preserved verbatim.

**Row 1 — filters** (unchanged controls, grouped by facet with the existing flex wrap):

```
[Scope ▾ ⓘ] [Level ▾] [Search ______] | [From ______ 📅] [To ______ 📅] | [Sort ▾] [☐ Follow]
```

- The retention/methodology sentence moves to the ⓘ info-tip on Scope, verbatim. The `#logMeta` div is deleted.
- The bundle warning moves to a `title`/info-tip on the Export diagnostics bundle button, verbatim.

**Row 2 — results bar** (footer strip of the same card, replacing the separate rows at `static.ts:2483–2488`):

```
[‹ Prev] [Next ›]  Page: 1 of 3  ▓▓▓▓░░░░░░  Matching: 1,234        [Limit ▾] [⟳ Refresh] [Export diagnostics bundle] [Open folder]
```

- Prev/Next, page indicator, and Limit (moved down from Row 1) form one pagination cluster.
- "Page: 1 of 3" and "Matching: 1,234" as Label: Value pairs; the total formatted with thousands separators (fixes Q14).
- A ~64px position micro-bar between them, fill = page / pages (fixes Q8 without inventing data).
- Refresh and Export/Open folder move here — actions grouped with the results they act on, out of the filter row (fixes Q7).

**Empty states:** the results body shows one short line ("No matching lines." / "No lines yet."); the explanatory clause moves into the Scope info-tip with the rest of the methodology (fixes the Q13 pattern in the body).

Net effect: the permanent warning line (~20px) and the meta line (~18px + margins) leave the page, the separate pagination row is absorbed into the card, and no datum changes — only its placement and formatting.

## Calendar date/time picker (in-flight addition) — assessment

**It fits the principles.** Current state in the tree:

- Buttons `#logFromCal` / `#logToCal` (`static.ts:2459`, `static.ts:2462`) sit beside the From/To inputs, which remain native `datetime-local` and fully typeable (`static.ts:2458`, `static.ts:2461`) — typed entry is preserved, as required.
- The picker (in `wireLogs`, from `static.ts:2536`) only composes a value, writes it into the input, and dispatches the input's `change` event, so applying from the picker is indistinguishable from a manual edit, including the page-1 reset — no second filter path, no duplicated state (principles 2, 6).
- Compact (principle 1): two small icon buttons in the existing row; the popup is fixed-positioned from the anchor's rect so the card cannot clip it, and it extends the page by nothing.
- Real data (principle 6): the grid's `sel` / `today` / other-month cell states correspond to the actual selected date, the actual current date, and real adjacent-month dates.
- No narrative is added: the only copy is the button tooltip "Pick a date and time" and a "Time" noun label in the popup (principles 5, 9).

Two watch items, neither a violation: (1) WebView2's native `datetime-local` control has its own picker affordance, so there are two entry points to the same value — acceptable, since the native one cannot be removed or styled, and both converge on the same input; (2) the popup re-renders on month navigation and preserves typed time via an explicit sync step — behavior to re-verify once the other agent's work lands, not a design-principle issue.

One design-consistency note for Row 1 above: with the calendar buttons present, each From/To group is label + input + button; keep the button inside the label's flex group in any re-layout so the pairing survives wrapping.

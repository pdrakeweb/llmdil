# Design Audit — Activation Plan Card

**Subject:** `activationPlanHtml` in `ui/src/views/static.ts` (lines 579–621), audited against the 14-question checklist in `docs/design-principles.md` Part 3.
**Method:** source read of the renderer, its helpers (`planChangeMeta` 570–577, `manualCommandHtml` 562–566), its render sites, and its data type (`ActivationPlanData`, `ui/src/api.ts:1363–1380`). No code changed.

## Where the card actually renders

`activationPlanHtml` is painted in three places, all with the same full markup:

- Activation Plan view — `wireActivationPlan` paint, `static.ts:780`, stacked directly above the separate Pending changes table card (`static.ts:774–779`).
- Backends view — into `#backPlan` above the add form, `static.ts:1247–1250`.
- Gateway view — concatenated at the head of the gateway paint, `static.ts:2972`.

The top-of-window activation banner is **not** this component: it is a second, hand-written rendering of the same plan data in `paintActivationBanner`, `ui/src/main.ts:76–94`. Its Apply path also behaves differently — it only ever runs mode `"restart"`/`"start"` (`main.ts:125`), never the drain mode the card offers (`static.ts:607`).

## Scorecard

Every checklist answer must be "no"; a "yes" is a failure.

| # | Question | Verdict | Evidence |
|---|---|---|---|
| 1 | Compact | **FAIL** | Full per-change list renders inline in every placement (`static.ts:590–591`), growing the card unboundedly and pushing Backends/Gateway content down. A collapsed form already exists (the dismissed variant, `584–588`) but is only reachable via "Decide later". |
| 2 | One section, multiple views | **FAIL** | In the Activation Plan view the card's change list stacks directly above the Pending changes table (`static.ts:780` + `774–777`) — the same data as two stacked blocks, not views of one section. The banner (`main.ts:76–94`) is a third independent rendering of the same data rather than a compact view of this component. |
| 3 | Prime real estate | **FAIL** | Top element is a prose heading, "Activation plan — saved changes are not live" (`static.ts:612`). The card's question — how much is not live, and what do I do — is answered by no visual and no headline value; the count `n` (`581`) is never surfaced in the expanded card (only in the dismissed one-liner, `586`). |
| 4 | Narrative in tooltips | **FAIL** | Three inline methodology/explanation blocks: the state line ("Apply restarts what is running (backends first, then the gateway); changes whose process is stopped take effect at its next start." / "Nothing running holds the old state…", `594–596`); the drift sentence (`592–593`); the cancel explanation ("Cancelling restores the configuration from before these changes and clears the plan.", `618`). |
| 5 | Real data | pass | The card has no chart elements. Its one color, the amber border (`585`, `611`), corresponds to a genuinely transitional state. (The *absence* of visuals is scored under Q3/Q8, not here.) |
| 6 | Numbers in one place | **FAIL** | In the Activation Plan view, every change's timestamp, summary, scope, targets, and activation appear in the card list (`590–591`) and again in the Pending changes table (`775`). The banner separately repeats the count (`main.ts:86`) above the same view. |
| 7 | Density with order | **FAIL** | Order is heading → full per-change detail → consequence summary (state line) → actions (`590–596`). Detail precedes summary; the principle is summary first, detail on demand. |
| 8 | Multi-number data is a chart | **FAIL** | The plan is inherently multi-number — N changes split by scope (backend/gateway), by activation (restart required / next start), by target — and all of it is text: the list (`590–591`) and the meta string (`planChangeMeta`, `570–577`). Segmented-bar primitives exist in `ui/src/charts.ts` and are unused here. |
| 9 | Label: Value | **FAIL** | Visible sentences: the heading clause (`612`), the state line (`594–596`), the drift sentence (`592–593`), the cancel explanation (`618`). `planChangeMeta` ("Backend: coder · restart required", `575–577`) is label-shaped and passes; the sentences do not. |
| 10 | Copy buttons | **FAIL** | The manual-command block (`620`, via `manualCommandHtml`, `562–566`) is a multi-line shell command the user is expected to run by hand, rendered as a bare `<pre>` with no copy button — and `word-break:break-all` (`564`) makes manual selection error-prone. This is exactly the value class principle 10 protects. |
| 11 | Status colors | pass | Amber border + warn icon for a pending/transitional state matches the amber semantics; error status text uses `var(--err)` (`619`); no chip states a default; no green/red misuse. |
| 12 | Unknown is not zero | pass | No measured quantities are rendered. The zero-changes case has a dedicated drift branch (`592–593`) rather than rendering "0 changes"; timestamps are times, not measurements (`fmtTime`, `113–116`). |
| 13 | Empty states | **FAIL (minor)** | Top level is compliant (no plan → `""`, `580`), and status/manual blocks render only when present (`619–620`). But when the journal is empty and only drift exists, the change-list block renders an explanatory sentence (`592–593`) instead of a Label: Value line or an omitted block. |
| 14 | Number formatting | pass | The only numbers are the integer change count and locale time strings; no decimals anywhere in the card. |

**Result: 10 of 14 questions fail** (Q1, Q2, Q3, Q4, Q6, Q7, Q8, Q9, Q10, Q13-minor). Passes: Q5, Q11, Q12, Q14.

## Findings in brief

The card is prose-led: warning heading → full text list of changes → methodology paragraph → buttons. Its single most consequential defect is the combination of Q3/Q6/Q2 in the Activation Plan view: the card leads with no headline value or visual, then repeats — verbatim — the changes table rendered immediately below it, so the view's prime real estate is spent saying everything twice in text. The second structural defect is placement blindness: the identical full card (list included) is injected into the Backends and Gateway views (`static.ts:1250`, `2972`), where a pending plan should cost one line, not a growing list. The banner shows the app already knows the right summary shape ("N saved changes (backend, gateway) not live yet" + Review/Apply, `main.ts:86–93`) — it is simply a separate implementation instead of a view of this component (and its Apply silently lacks the drain option).

What is already right and should survive any redesign: the amber pending semantics (Q11), the omitted-when-nothing-pending behavior (Q13 top level), `planChangeMeta`'s label-shaped meta string, the armed-confirm pattern for Cancel, and the dismissed one-line form — which is the natural seed of the summary variant below.

## Proposed redesign

One component, two views (principle 2), selected by a `variant` parameter on `activationPlanHtml`. The banner in `main.ts` is replaced by the summary variant, deleting `paintActivationBanner`'s bespoke markup.

### Summary variant (banner, Backends view, Gateway view)

One line, no list, no prose:

- Warn icon (amber, as now).
- Headline value: **"Pending changes: 3"** as Label: Value — the count becomes the card's answer (Q3).
- Scope composition as a mini segmented bar (charts.ts primitive): backend segment vs gateway segment, widths proportional to the per-scope counts — real data, chart keys "Backend · Gateway" in the bar's tooltip or adjacent key labels (Q8, Q5). For the drift-only state (0 journal changes): a single gray segment keyed "Gateway drift".
- Actions: **Review** (navigates to the full view), **Apply** when `actionable` — with the same mode semantics as the full view, so banner Apply and card Apply stop diverging; if a drain choice is offered in the full view, the banner's Apply tooltip states which mode it runs.
- The state line's content moves to the Apply button's tooltip (Q4): "Restarts running backends first, then the gateway; stopped processes pick changes up at their next start."
- Status/manual-command feedback renders below the line only when present, as today; the manual command block gains a copy button (Q10) in both variants.
- "Decide later" collapses the inline placements to this same one-liner (converging the dismissed variant, `584–588`, with the summary variant — one collapsed shape, not two).

### Full variant (Activation Plan view)

- Header row: noun label **"Activation plan"** + tiles: **"Pending changes: N"**, **"Restart required: N"**, **"Next start: N"** (Label: Value single values), and the scope segmented bar with key labels — each number in exactly one place (Q6, Q9). The tiles are aggregates; the table below holds per-change facts only, so nothing repeats.
- The changes table (existing, `static.ts:774–777`) becomes the detail view of the same section: either the section's second tab ("Summary | Changes") or a collapsed-by-default expandable under the tiles. The card's inline change list is deleted — it is the table, duplicated (Q1, Q2, Q6).
- Order becomes summary (tiles + bar) → actions → detail on demand (Q7).
- Drift state: the tiles row shows **"Gateway config: drifted"** as a Label: Value with an info-tip explaining drift; no sentence (Q9, Q13).
- Cancel: the explanation sentence becomes the Confirm button's tooltip (Q4); the armed-confirm button swap is unchanged.
- The live gateway line in the detail card ("Gateway: running · …(drift)", `static.ts:778`) is already Label: Value and folds into the detail view unchanged.
- Empty state (no plan pending): keep omitting the card; the view's "No activation plan pending" card keeps its Refresh action but its explanatory paragraph (`static.ts:768–772`) moves to an info-tip on the heading, leaving "Pending changes: 0" as the visible value.

### Element mapping

| Current element | Becomes |
|---|---|
| Heading sentence (`612`) | Noun heading "Activation plan" + count tile |
| Change list (`590–591`) | Deleted from the card; the existing changes table is the detail view (tab/expandable) |
| State line (`594–596`) | Tooltip on the Apply buttons |
| Drift sentence (`592–593`) | "Gateway config: drifted" Label: Value + info-tip |
| Scope/activation meta text (`570–577`) | Segmented bar (scope) + count tiles (activation); meta string retained inside table rows |
| Cancel explanation (`618`) | Tooltip on Confirm cancel |
| Manual command `<pre>` (`562–566`) | Same block + copy button, both variants |
| Dismissed one-liner (`584–588`) | Merged into the summary variant |
| Banner markup (`main.ts:76–94`) | Replaced by the summary variant; banner Apply gains the full view's mode semantics |

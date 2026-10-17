# Design Audit — Pending Changes Card

**Card:** Pending changes card (changes table)
**Renders in:** Activation Plan view, below the activation plan card (`activationPlanHtml`)
**Source:** inline in `wireActivationPlan`'s `paint`, `ui/src/views/static.ts:757–805` (card markup at 766–782)
**Principles:** `docs/design-principles.md` (14 principles + Part 3 checklist)
**Verdict:** Redesign needed — 9 of 14 checklist questions fail (6 hard, 3 minor).

## Scorecard

| # | Question | Result | One-line note |
|---|---|---|---|
| 1 | Compact | Pass | A table is the compact form for itemized records in a dedicated view; length is data-driven. |
| 2 | One section, multiple views | **Fail** | The same changes render twice in the same view: prose list in the plan card above, table here. |
| 3 | Prime real estate | Pass | The card's first element after its heading is the table — the data that answers its question. |
| 4 | Narrative in tooltips | **Fail** | Three inline prose sites: empty-state paragraph, drift table row, drift clause on the gateway line. |
| 5 | Real data | Pass | The card has no bars/fills/colors; nothing encodes a value falsely (absence of visuals is Q8, not Q5). |
| 6 | Numbers in one place | **Fail** | Every change's summary, scope, targets, activation, and time appear in both cards; drift is stated up to three times in one view. |
| 7 | Density with order | Fail (minor) | The card's one summary datum (gateway up/down) sits in a footer line below the detail table. |
| 8 | Multi-number data is a chart | Fail (minor) | Scope/activation counts (e.g. 3 restart · 2 next-start) are multi-number data rendered nowhere as a visual. |
| 9 | Label: Value | **Fail** | The Change column is journal prose sentences; the structured `kind` field exists and is never rendered. |
| 10 | Copy buttons | Pass | No URL, key, or address appears in this card. |
| 11 | Status colors | Fail (minor) | State-bearing values (Restart vs Next start, gateway down, drift) render as bare gray text with no chips or color semantics. |
| 12 | Unknown is not zero | **Fail** | A failed plan fetch renders as the definitive all-clear: "No activation plan pending / Every saved configuration change is live." |
| 13 | Empty states | **Fail** | The no-plan state is a full explanatory paragraph; the drift-only state is a prose sentence inside an otherwise empty table. |
| 14 | Number formatting | Pass | No decimals are rendered; `fmtTime` (static.ts:113–116) yields `HH:MM:SS`. Nit: timestamps carry no date (see Nits). |

## Findings (failures, with evidence)

### Q12 — A failed fetch paints "every change is live" (top gap)

`getActivationPlan` catches invoke errors and returns `EMPTY_PLAN` (`pending: false`) with the error attached (`ui/src/api.ts:1382–1396`). `wireActivationPlan`'s `load` ignores `r.error` entirely — `lastPlan = r.data; paint();` (static.ts:785–790). `paint` then takes the not-pending branch and renders "No activation plan pending" over the sentence "Every saved configuration change is live." (static.ts:766–769). An unknown state is presented as a measured, resolved fact — the exact class of failure principle 12 exists to prevent, and the most dangerous one in this card: after any transient backend error, the view affirmatively tells the operator there is nothing to apply. (The code review of v0.1.30→v0.1.32 flagged the same path as a Major.)

### Q2 + Q6 — The card duplicates the plan card above it, field for field

`paint` renders `activationPlanHtml(p, planUi) + detail` (static.ts:782). The plan card's `changeList` maps every change to its summary plus `planChangeMeta(c)` — scope, targets, activation — plus `fmtTime(c.ts)` (static.ts:596–598, 571–579). The pending changes table then renders the same five fields again as columns (static.ts:777, 780). On this view the operator reads every change twice, once as prose and once as a table: two stacked representations of one dataset (Q2), and every value in two places (Q6). Drift is worse: the plan card states it (static.ts:598), the table restates it as its fallback row (static.ts:780), and the card's own gateway line states it a third time (static.ts:780).

### Q4 + Q9 + Q13 — Prose where values belong

- **Empty state (Q4, Q13):** the not-pending branch is a heading over a three-line methodology paragraph — "Every saved configuration change is live. A change that needs a restart to take effect — gateway routes, a backend's flags, tensor split, or model assignment — appears here, and in the banner at the top of the window, until it is applied." (static.ts:769). Principle 13: a block with no data is omitted, not explained; principle 5: explanation belongs in a tooltip.
- **Change column (Q9):** rows render `esc(c.summary)` verbatim (static.ts:777) — journal sentences such as "Backend `coder` expert offload set to 40 of 48 MoE layers". The payload already carries a structured `kind` per change (`ActivationChange.kind`, api.ts:1342–1345) that no surface renders; the sentence is doing the kind's job, so the column cannot be scanned as Label: Value.
- **Drift row (Q4, Q13):** when the plan is pending on drift alone, the table body is a single sentence in a `colspan=5` cell — "The running gateway's configuration differs from the saved configuration (drift)." (static.ts:780) — placeholder prose inside a data table.
- **Gateway line (Q4):** `Gateway: running` is proper Label: Value, but the drift clause is appended as inline prose: "· the saved gateway configuration differs from the running one (drift)" (static.ts:780).

### Q11 — State values carry no color semantics (minor)

Scope ("Backend"/"Gateway") and Activation ("Restart"/"Next start") are plain text (static.ts:777). "Restart" means a live process must bounce to apply — action-required/transitional information that principle 11 encodes in amber — and "Next start" is neutral/gray information; the card renders them identically. Likewise "Gateway: not running" and drift get no visual flag anywhere in this card (the amber border lives on the plan card above).

### Q7 — Summary after detail (minor)

The card's only summary-level datum — whether the gateway is up, and whether it has drifted — is a footer line under the table (static.ts:780), after the per-change detail it contextualizes. Grouping is otherwise by question (one table for all changes), which is right.

### Q8 — No visual summary of the card's own counts (minor)

The change set has real multi-number structure — counts by scope (backend vs gateway) and by activation (restart vs next start), and the plan's actionability — none of which is rendered as any visual anywhere in the card. A one-line segmented bar over the table (segments = changes by activation class) would encode it; today the numbers exist only as row counts the reader must tally.

## Redesign proposal

Merge the plan card and this card into **one section** on the Activation Plan view: a summary header (the existing plan card, minus its prose change list) over this card's table as the single detail representation. The banner keeps its compact summary + Review button; only the dedicated view changes.

**Layout (top to bottom):**

1. **Header line:** title "Pending changes" + count as Label: Value ("Changes: 5") + status chips — gateway state as a chip ("Gateway: running", green; "Gateway: stopped", light red) and, only when present, an amber "Drift" chip whose tooltip carries the current drift sentence verbatim. Gateway state moves here from the footer (fixes Q7, Q11).
2. **Summary visual:** one segmented bar, full card width, one segment per change colored by activation class — amber = restart required, gray = applies at next start — with the counts as the bar's key ("Restart: 3 · Next start: 2"). This is the card's multi-number data as a chart (Q8), and it is the *only* place those counts appear (Q6).
3. **Changes table** (the detail, unchanged in role): columns **Saved | Target | Change | Activation**.
   - *Target* absorbs Scope: "Backend: coder" / "Gateway" (Label: Value, no separate Scope column).
   - *Change* renders structured: the change `kind` as the label and the value core as the value (e.g. "Offload: 40 of 48 layers"), derived from `kind` + `targets` + the numeric content the journal already puts in `summary`. Where a kind has no structured form yet, the full journal summary moves to the row's tooltip/expandable instead of the cell.
   - *Activation* renders as a chip: "Restart" amber, "Next start" gray (Q11).
   - *Saved* shows date + time (`fmtDate` + `fmtTime`) — see Nits.
4. **Actions** stay in the section header (the plan card's Drain Requests and Apply / Apply now / Decide later / Cancel changes), not duplicated in this card. The card's own Refresh button is removed: the view already re-polls every 10 s (static.ts:800), and one Refresh in the header covers manual reloads.

**Tooltip / expandable moves:** the empty-state methodology paragraph becomes an info-tip on the section title; the drift sentence becomes the Drift chip's tooltip; per-row journal prose becomes a row expandable (or tooltip) for the kinds without a structured rendering.

**Empty and error states:**
- *No plan pending:* the section renders a single line — "Pending changes: 0" with the info-tip — plus status/manual-command output when an action just ran. No paragraph (Q13, Q4).
- *Drift only (no journal changes):* header + Drift chip + summary bar omitted; the table is omitted entirely rather than holding a prose row (Q13).
- *Fetch failed:* the section renders "Activation plan: unavailable" with the error in the status line — never the all-clear. `load` must keep the last good plan and surface `r.error` instead of adopting `EMPTY_PLAN` (Q12).

**Unchanged:** the 10 s poll, the plan actions and their execution paths, the banner surface, and the journal payload contract (the redesign consumes `kind`, which the payload already sends).

## Nits

- **Timestamps are time-only.** `fmtTime` renders `HH:MM:SS` with no date (static.ts:113–116); a journal entry from yesterday is indistinguishable from one from this morning. Show date + time in the Saved column.
- **Targets separator.** Multi-target rows join with ", " (static.ts:777); with the Target column absorbing scope, cap the visible list and put overflow in the cell tooltip.

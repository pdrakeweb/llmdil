# Design Audit — Backend Card

Card: Backend card · Renders in: Backends view, grouped under gateway-group sections · Source: `backendCard`, `ui/src/views/static.ts` (lines 880–1205 as read)
Reference: `docs/design-principles.md` (14 principles, Part 3 checklist)
Date: 2026-10-16 · Method: code inspection of the current tree. Line numbers are as read; the file is under concurrent edit by the UI-pass work, so lines may drift by a few.

## Verdict

**Redesign needed** — and already approved by Pete (2026-10-16): metric tiles, per-GPU residency bars, stats tiles only when data exists, a Details expandable, adopted chip green. That redesign closes 6 of the 11 gaps below, partially closes 1, and leaves 4 remainders that are **not** in its scope and need riders (G4, G8, G9, G11) plus one view-level decision (G5).

Score: Pass 2 · Minor gap 7 · Major gap 3 (questions can carry more than one gap; 11 gaps total: 3 Major, 7 Minor, 1 Nit).

## Scorecard (Part 3 checklist — every answer should be "no")

| # | Question (short) | Result | Evidence |
|---|---|---|---|
| 1 | Compact | **Major gap** — G2 | Closed card always renders the GPUs section, a stats block, and the State/PID/Uptime grid (static.ts:1046–1051, 1055–1063, 1176–1181) |
| 2 | One section, multiple views | Minor gap — G6 | Inactive branch renders the split segmented bar **and** % rows with the same fractions, stacked (static.ts:1031–1033) |
| 3 | Prime real estate | Minor gap — G1/G10 | First content row is an editing control (model `<select>`, static.ts:1168); the card's central visual data (residency) is the fourth block down |
| 4 | Narrative in tooltips | Minor gap — G8 | Most narrative already rides tooltips (adopted/orphan/basis/placement); the failed-state log tail still shows inline prose (static.ts:973, 975–976) |
| 5 | Real data | **Pass** | Split-bar widths are `frac * 100` (static.ts:1004); offload gradient band stops come from payload fractions `green_end`/`yellow_end` (static.ts:841–844); state dot maps 1:1 to state (static.ts:43–48) |
| 6 | Numbers in one place | Minor gap — G5 | State appears as dot + chip + "State" kv tile on one card (static.ts:1161–1167, 1176–1181); Requests/Errors (1h) appear in the card's tiles and again in the view's overview columns (static.ts:1055–1061 vs 1292–1311) |
| 7 | Density with order | Minor gap — G10 | Config-derived requirement figures precede live measured residency; State/PID/Uptime grid sits mid-card (static.ts:1172, 1176–1181) |
| 8 | Multi-number data is a chart | **Major gap** — G1, G7 | Per-GPU measured residency is a list of text rows (static.ts:1022–1025); in the measured branch the configured split is a text summary line with no bar (static.ts:1000–1002, 1050) |
| 9 | Label: Value | Minor gap — G8 | Card data lines comply ("GPU RAM: 11 GiB · System RAM: 0 GiB · KV cache: 2 GiB", static.ts:1172); the log-tail sentence and caption do not (static.ts:973, 975) |
| 10 | Copy buttons | **Pass** | The card carries no URL, key, or address value. The model file path (static.ts:1170) is a local path shown with a full-path tooltip — outside principle 10's categories. Nearest candidate, the launch-command `<pre>` block (static.ts:945–947), is a command line, not a listed category |
| 11 | Status colors | Minor gap — G9 | enabled/adopted green `status-on`, disabled gray `status-off`, stopped/failed light-red `status-bad`, starting/stopping amber `status-transition` (static.ts:873–878, 1163–1167; styles.css:204–207) — but "not in config" uses `badge-warn` (styles.css:384, amber `--warn`), and amber is reserved for transitional states |
| 12 | Unknown is not zero | **Major gap** — G4 | RAM line: `System RAM: ${fmtGb((b.sys_ram_mib ?? 0) / 1024)} GiB` (static.ts:1172) renders an absent figure as "0 GiB". `sys_ram_mib` is independently optional (`ui/src/api.ts:619`). Unmeasured residency itself complies ("not measured", no figure — static.ts:1024, 1030) |
| 13 | Empty states | Minor gap — G3, G8 | No-metrics-window renders a placeholder line "Gateway stats: —" + tooltip for every configured+enabled backend (static.ts:1063); a failed backend with an empty tail renders the sentence "No log lines captured for this backend." (static.ts:973) |
| 14 | Number formatting | Nit — G11 | `fmtGb`/`trim1` enforce the rule (charts.ts:132–142) and counts use `toLocaleString`; the Placement projection table prints raw MiB integers with no thousands separators ("13824 MiB", static.ts:~1115–1117) |

## Gap register

### G1 — Major · Q8/Q3 · Residency is text, not a visual
Measured per-GPU residency — the card's central multi-number data — renders as label/figure rows (`<div class="backend-gpu-row">`, static.ts:1022–1025). There is no bar in the measured branch at all; the only bar in the section is the configured-split bar in the inactive branch. Failure: with two backends resident on two GPUs, comparing "who holds what where" requires reading and mentally normalizing four numbers per card.
**Approved redesign: CLOSES.** Per-GPU horizontal bars directly under the header, fill = the backend's measured MiB on that GPU ÷ that GPU's total MiB (device share — a different denominator from the split fraction, and the bar must be labeled by the section basis, as today), GiB value at the right end. The text rows go away; the GiB figure lives at the bar end only (no repetition, per Q6).

### G2 — Major · Q1 · Closed card is long
Every configured card, closed, shows: model row, Context line, RAM line, GPUs section, stats block or placeholder, State/PID/Uptime grid, actions — before any expandable is opened. With five backends the view is a long scroll of full-detail cards.
**Approved redesign: CLOSES.** Closed card = header + metric strip + residency bars (+ stats tiles when present). State/PID/Uptime, split detail, and basis move into the single collapsed Details expandable.

### G3 — Minor · Q13 · Stats placeholder line
`Gateway stats: —` with an info-tip renders whenever a configured, enabled backend has no metrics window yet (static.ts:1063) — placeholder prose for an empty block, on the card's most common state (the window exists only after an app-driven gateway restart + ~30 s of polling).
**Approved redesign: CLOSES** — explicitly: stats tiles only when a window exists, otherwise the block is omitted entirely.

### G4 — Major · Q12 · Absent system-RAM figure renders as "0 GiB"
static.ts:1172 substitutes `0` for a missing `sys_ram_mib`. A backend whose payload carries `gpu_ram_mib` without `sys_ram_mib` (both are independently optional, api.ts:618–619) displays a confident, wrong "System RAM: 0 GiB" — indistinguishable from a genuinely zero requirement (dense models legitimately show 0). Likelihood is low because the requirement machinery computes both together, but the render must not invent the number.
**Approved redesign: REMAINDER.** The metric tiles consume the same payload; nothing in the approved spec changes the fallback. Rider: the System RAM tile is omitted (or shows "—") when `sys_ram_mib` is absent; "0 GiB" only when the value is present and zero.

### G5 — Minor · Q6 · Values shown twice
(a) State: header dot (color), state chip (text), and the "State" kv tile all carry the same value on one card (static.ts:1161–1167, 1176–1181). (b) Requests (1h) and Errors (1h) render in the card's stats tiles (static.ts:1055–1061) and again as columns in the Backends view overview list directly above the cards (static.ts:1292–1311).
**Approved redesign: PARTIAL.** (a) The kv grid moves into the Details expandable, so the closed card shows state once (chip; the dot is a color key, acceptable); the expandable still repeats the chip's word — acceptable inside on-demand detail, or drop "State" from Details since the chip carries it. (b) **Remainder — view-level decision the card redesign does not make:** keep the 1h request/error numbers in the overview list or in the card tiles, not both. Recommendation: overview keeps them (it's the cross-backend comparison surface); card tiles then carry only Throughput, or the tiles move wholly to the overview and the card drops stats.

### G6 — Minor · Q2/Q6 · Split shown as bar and rows, stacked
Inactive branch: `gpuRows = splitBar + rows` (static.ts:1031–1033) — the segmented bar's segment widths and the rows' percentages are the same fractions rendered twice in one block.
**Approved redesign: CLOSES**, provided the ghost (outline) bars *replace* the % rows rather than joining them; percentages move to the Details expandable as Label: Value lines.

### G7 — Minor · Q8 · Configured split as a text line (measured branch)
When measured rows exist and a split is configured, the card appends `Configured split: GPU 0 · … — 100% · GPU 1 · … — 50%` as a prose-ish text line (static.ts:1050) — multi-number data with no visual, duplicating what the Placement panel edits.
**Approved redesign: CLOSES**, with the same caveat as G6: the line must be removed once bars exist (ghost/secondary encoding or Details), not kept beside them — kept, it becomes a fresh Q6 violation.

### G8 — Minor · Q4/Q9/Q13 · Failed-state log tail prose
A failed backend with a fetched-but-empty tail renders the sentence "No log lines captured for this backend." (static.ts:973), and a non-empty tail is headed by the caption "Latest output (stderr preferred)" (static.ts:975–976) — neither is Label: Value, and the empty case is placeholder prose for an absent block.
**Approved redesign: REMAINDER** (the redesign doesn't cover the log tail). Rider: omit the tail block entirely when the tail is empty; head a non-empty tail with a compact label ("Latest output") and move "(stderr preferred)" into its tooltip. Transient action-result messages (`back-msg`, static.ts:1196) were a deliberate exception in the earlier pass and stay out of scope.

### G9 — Minor · Q11 · "Not in config" chip color is outside the semantic set
The orphan chip uses `badge-warn` = `var(--warn)` amber (styles.css:384). Principle 11 assigns amber to transitional states (starting/stopping); an orphan is a persistent state. The chip itself is correct — orphan is non-default information and its counterpart correctly does not exist (static.ts:1163) — only the color breaks the mapping.
**Approved redesign: REMAINDER.** Rider (design-doc decision, then one class change): either render it `status-off` gray with the warning carried by its tooltip, or amend principle 11 to define a warning class explicitly. Note the adopted chip needs nothing: it already renders green `status-on` (static.ts:1166), so the redesign's "adopted chip green" item is already satisfied in current code.

### G10 — Minor · Q7/Q3 · Ordering: config before live, detail mid-card
The RAM requirement line (config-derived) precedes measured residency (live), and the State/PID/Uptime detail grid sits between the stats block and the actions (static.ts:1172–1181). Summary-first would order: state (header) → live residency → requirements → stats → detail.
**Approved redesign: CLOSES** — its layout (header → metric tiles → residency bars → stats tiles → Details) is exactly this order.

### G11 — Nit · Q14 · MiB figures without thousands separators
The Placement projection table prints `${p.per_gpu_mib[i]} MiB` and `${p.free_mib[i]} MiB` raw (static.ts:~1115–1117) — five-digit counts with no separators, against principle 14's separator rule for counts.
**Approved redesign: REMAINDER.** Rider: format with `toLocaleString()` like the other counts; one-line change, inside the Placement panel rather than the card body.

## Proposed redesign (approved design, concretized, plus riders)

```
┌──────────────────────────────────────────────────────────────┐
│ ● planner  :8081        [enabled] [adopted] [running]        │  header: dot + chips (existing classes)
│ ┌────────┬────────┬────────┬────────┬────────┐               │
│ │ Model  │ Context│ GPU RAM│ Sys RAM│ KV     │  metric tiles │  single values, existing payload fields
│ │ Qwen3… │ 32,768 │ 11 GiB │ 0 GiB  │ 2 GiB  │               │
│ └────────┴────────┴────────┴────────┴────────┘               │
│ GPU 0 · RTX A4000    ████████████████░░░░  13.8 GiB          │  bar fill = measured MiB ÷ GPU total MiB
│ GPU 1 · Tesla P100   ▌                     0.3 GiB (ctx)     │  sub-1 GiB measured = sliver; tooltip "context only"
│ [Requests (1h) 1,234] [Errors (1h) 2] [Throughput 41 tok/s] │  only when a metrics window exists (G3)
│ ▸ Details                                                    │  collapsed: State/PID/Uptime, configured
│   [Start] [Restart] [Test] [≡]                               │  split %, model file; actions row as today
└──────────────────────────────────────────────────────────────┘
```

- **Becomes a visual:** per-GPU residency (bars, G1); configured split on a stopped card (same bars, outline/ghost fill = split share of the model, percentages demoted to Details, G6/G7). Bar colors stay the per-GPU categorical palette already used by the split bar (static.ts:993) so a GPU's color is stable across cards.
- **Becomes Label: Value tiles:** Model, Context, GPU RAM, System RAM, KV cache — the values currently on static.ts:1168–1172, unchanged, split into tiles. System RAM tile omitted when the payload lacks the figure (G4 rider).
- **Moves to tooltip/expandable:** State/PID/Uptime and configured-split percentages → Details expandable; basis stays a tooltip on the RAM tile (as today, static.ts:1156–1163); adoption/orphan explanations stay chip tooltips (as today).
- **Omitted when empty:** stats block (G3), log-tail block when the tail is empty (G8 rider), System RAM tile when unmeasured (G4 rider), GPUs section for unconfigured backends (already the case, static.ts:1046).
- **Unchanged:** action buttons and their offer rules, hamburger menu, Flags panel (incl. the offload slider — its gradient already encodes the catalog band fractions, static.ts:841–844), Placement panel and projection table (plus the G11 separator rider), log tail content when non-empty.
- **Open decision (G5b):** Requests/Errors (1h) — overview list or card tiles, not both.

## What already complies (no action)

- Adopted chip is green (`status-on`, static.ts:1166); no "not adopted" chip exists. State color mapping matches principle 11 throughout (static.ts:873–878; styles.css:204–207).
- Unmeasured residency renders "not measured" with no figure and no context-only qualifier (static.ts:1024, 1030–1032); the context-only qualifier is applied only to a measured sub-1 GiB value (static.ts:1023).
- All narrative that the 2026-10-16 de-wordify pass targeted is in tooltips: adopted (static.ts:1166), orphan (1163), basis (1172), not-measured section (1048), placement explanations (projHtml/placeHtml).
- Number formatting via `trim1`/`fmtGb` (charts.ts:132–142) satisfies principle 14 everywhere except G11.

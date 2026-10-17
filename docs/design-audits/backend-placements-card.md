# Design Audit — Backend Placements Card

Card: Backend placements card, Dashboard (Appliance view)
Source: inline in `applianceView`, `ui/src/views/dashboard.ts:170-227`; data from `placementFromConfig`, `ui/src/api.ts:245-283`; bar primitive `stackedBar`, `ui/src/charts.ts:62-77`; styles `ui/src/styles.css:279-285`.
Principles: `docs/design-principles.md` (14 principles, Part 3 checklist).
Method: code inspection only; no code changed.

**Verdict: minor gaps — 4 of 14 checklist items fail (Q4, Q5, Q11, Q13).** The core design is sound: one stacked bar per backend answers "where does each backend's model live across the GPUs", percentages live only in the bar, and disabled rows are grayed. The failures are localized: placeholder prose in the empty state, a bar whose widths are renormalized shares while its tooltips state raw percentages, a default placement rendered identically to a configured one, and eventual treatment applied to disabled backends but not to stopped ones.

## Scorecard

| # | Checklist question (answer must be "no") | Result | Evidence |
|---|---|---|---|
| 1 | Compact — anything that belongs behind a tab/toggle/expandable? | Pass | One row per backend (label + bar), `dashboard.ts:178-184`; nothing expandable-worthy is inline. |
| 2 | One section, multiple views — same data two ways stacked? | Pass | Split data appears once, as the bar; the legend is the bar's key, not a second representation (`dashboard.ts:189-199`). |
| 3 | Prime real estate — top element anything other than the answering visual/value? | Pass | After the card title, the first element is the first backend's placement bar (`dashboard.ts:227, 182-183`). |
| 4 | Narrative in tooltips — explanation visible inline? | **Fail** | The no-backends state renders two sentences of prose inline (see Q13). Separately, what "eventual" means is explained nowhere in the UI — only in a code comment (`dashboard.ts:174-177`); the `disabled · eventual` tag (`dashboard.ts:182`) carries no tooltip. |
| 5 | Real data — any mark failing to correspond to an actual value? | **Fail** | Widths encode renormalized shares, tooltips state raw percentages (detail below); sub-1% shares are dropped from the visual entirely; a no-split default is drawn identically to a configured 100% split. |
| 6 | Numbers in one place — any number shown twice? | Pass | Each percentage exists exactly once, in the segment's SVG `<title>` tooltip (`dashboard.ts:183`, rendered by `charts.ts:71`). The legend repeats GPU *names* (chart keys), not numbers. |
| 7 | Density with order — grouped by anything but the question; detail before summary? | Pass | Rows grouped by backend (the question the card answers); the legend follows the rows it keys (`dashboard.ts:199`). |
| 8 | Multi-number data is a chart — related numbers as text? | Pass | The per-GPU split — the card's only multi-number data — is the stacked bar itself. |
| 9 | Label: Value — any sentence, any sentence-as-label? | Pass (card body) | Row labels are identifiers (backend id + model stem, `dashboard.ts:182`); the `disabled · eventual` tag is a compound state key, not a sentence. The one sentence in the card is the empty state, counted under Q4/Q13. |
| 10 | Copy buttons — any URL/key/address without one? | Pass (n/a) | The card contains no URL, key, or address. |
| 11 | Status colors — semantics broken; chip stating the default? | **Fail (partial)** | Disabled is correctly gray (below), but an enabled-yet-stopped backend — whose placement is equally not in effect — renders at full palette saturation with no distinction, even though the row data already carries `backendStatus` (`api.ts:276`) and the card ignores it. |
| 12 | Unknown is not zero — unmeasured rendered as 0 or as measured? | Pass | Shares come from configuration, never from telemetry, so nothing unmeasured is presented. No zero values render: zero/rounded-to-zero segments are filtered (`api.ts:264`). (The default-as-configured issue is counted under Q5.) |
| 13 | Empty states — placeholder prose instead of omission? | **Fail** | `dashboard.ts:204-208`: when no backends exist the card still renders, with "No backends configured — placements appear here once backends exist. Set a split from a backend's Placement form." Principle 13 requires the block to be omitted. The page already has a top-level empty card for this situation (`dashboard.ts:213-215`), so the prose is also redundant. |
| 14 | Number formatting — >1 decimal, trailing `.0`, decimals ≥100? | Pass | Percentages are integers end to end (`Math.round`, `api.ts:262`); no decimals anywhere in the card. |

## Q5 detail — what the bar encodes, and whether the encoding is exact

The bar is *intended* to encode each GPU's configured share of the backend's tensor split. Three deviations make the encoding inexact:

1. **Widths and labels use different values.** `placementFromConfig` rounds each config fraction to an integer percent (`api.ts:262`), and `applianceView` then draws widths from the *renormalized* share — `(s.pct / total) * 100`, where `total` is the row's rounded sum (`dashboard.ts:179, 183`) — while the segment's tooltip label prints the *raw* rounded percent: `` `${s.gpu} ${s.pct}%` `` (`dashboard.ts:183`). Whenever the rounded shares don't sum to exactly 100 (e.g. a three-way split of 33/33/33, sum 99), every width is silently scaled by 100/99 while every tooltip still states the unscaled number. The chart and its own values disagree — the exact failure principle 5 names.
2. **Small shares vanish.** A share below 0.5% rounds to 0 and is filtered out (`api.ts:262-264`); the renormalization then stretches the surviving segments to fill 100%, so the bar asserts a complete split that omits a real (if small) allocation.
3. **A default is drawn as a configuration.** A backend with no configured split (or no positive entry) gets a fabricated single segment — GPU 0 at 100% (`api.ts:265-267`) — rendered with the same solid palette fill as an explicitly configured 100% split. The visual cannot distinguish "operator configured 100% on GPU 0" from "no split set; unsplit launches land on GPU 0 by default".

What is exact: segment order and color are stable per GPU position (`segmentGpuPos`/`gpuColor`, `dashboard.ts:158-168`), the legend is built from the same mapping, and relative proportions between segments of a row are preserved under the shared scaling — so the *shape* is right; it is the absolute correspondence between width and stated percent that breaks.

## Q11 detail — how disabled/eventual reads, visually vs by text

Disabled reads consistently in both channels, and this is the card's best state handling:

- Visually: every segment forced to a single gray `#6d7686` regardless of GPU (`dashboard.ts:183`), the whole row at 55% opacity (`.place-row.eventual { opacity: .55 }`, `styles.css:282`), and the row's GPUs excluded from the legend so the gray asserts no color key (`dashboard.ts:189-195`).
- By text: a bordered tag reading `disabled · eventual` beside the name (`.eventual-tag`, `styles.css:283`).

Gray for disabled matches principle 11 exactly. The gap is asymmetry: `backendStatus` is present in the row data (`api.ts:276`) but never read by the card, so an enabled backend that is *stopped* — placement equally not in effect until its next start — renders in full palette color at full opacity, visually claiming a live placement. Only the disabled state gets the eventual treatment.

## Redesign proposal

No structural change — the card's shape (one bar per backend + legend) already satisfies principles 3, 7, and 8. Four targeted fixes:

1. **Omit the card when there are no placements (Q13, Q4).** When `m.placement.length === 0`, render nothing for this card (the grid then holds only the throughput chart when a series exists). Delete the `placeEmpty` prose (`dashboard.ts:204-208`); the page-level empty card already covers the no-backends case. Any guidance about setting a split belongs in the Backends view, not here.
2. **Make the bar encode one value, exactly (Q5).** Carry the unrounded config fraction through to the bar: width from the fraction, tooltip percent computed from the same fraction (integer when whole, one decimal otherwise, per principle 14). Drop the renormalize-then-label-raw mismatch: if a normalization is ever needed, the tooltip must state the normalized figure — chart and value are the same number, shown once. Render sub-1% shares as a minimum-width sliver with the true value in the tooltip instead of dropping them and stretching the rest.
3. **Distinguish the default placement (Q5).** When a row exists only because of the no-split fallback (`api.ts:265-267`), render its bar in the ghost/outline treatment (reduced opacity, no legend entry — the legend is keyed to configured GPUs) and tag the row `unsplit` as a tooltip on the bar: "No split configured — an unsplit launch loads on GPU 0." A configured 100% split keeps the solid fill. The default stops reading as an operator's configuration.
4. **Apply eventual treatment by actuality, not just enabled-ness (Q11, Q4).** Key the de-emphasis off the state the data already carries: disabled rows keep today's exact treatment (gray segments, 55% opacity, `disabled · eventual` tag). Enabled-but-not-running rows (`backendStatus` present and not `running`) get the same gray segments and opacity with the tag `stopped · eventual` — the placement takes effect at the backend's next start, which is the same eventual semantics. Running rows keep palette colors, and the legend is built from running rows only. Finally, give both eventual tags a `title` tooltip carrying the explanation that today exists only in a code comment: "Placement takes effect when the backend is enabled and started." — narrative in a tooltip, per principle 5 of the principles doc (checklist Q4).

Everything else stays: row labels (identifiers, not sentences), legend construction, integer formatting, and the one-number-one-place discipline the card already observes.

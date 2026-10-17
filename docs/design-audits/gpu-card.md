# Design audit — GPU card

Scope: `gpuCard` in `ui/src/views/static.ts` (lines 134–220), rendered by `wireGpus`/`paintBody` in the GPUs view. Audited against the 14-question checklist in `docs/design-principles.md` Part 3. Audit only — no code changed. Date: 2026-10-16.

## Verdict

**Redesign needed (moderate).** The card's data integrity is strong — unmeasured per-process data is handled exactly right, expandable process detail already exists, empty sections are omitted — but its presentation layer is the pre-redesign pattern: the two headline numbers are each rendered twice (gauge ring + metric row), the two genuinely multi-number datasets (per-process VRAM, per-model residency) are plain text, and three pieces of narrative sit inline. 10 of 14 checklist questions have findings.

| # | Question | Result |
|---|---|---|
| 1 | Compact | Fail (minor) |
| 2 | One section, multiple views | Fail |
| 3 | Prime real estate | Pass |
| 4 | Narrative in tooltips | Fail |
| 5 | Real data | Pass |
| 6 | Numbers in one place | Fail |
| 7 | Density with order | Fail (minor) |
| 8 | Multi-number data is a chart | Fail |
| 9 | Label: Value | Fail |
| 10 | Copy buttons | Pass |
| 11 | Status colors | Fail (minor) |
| 12 | Unknown is not zero | Pass, with one minor edge |
| 13 | Empty states | Pass |
| 14 | Number formatting | Fail (minor) |

## Findings by question

### 1. Compact — Fail (minor)
The per-process table and the resident-models list are always fully expanded inside the card (`static.ts:192–204`). The process list is scroll-capped (`max-height:176px`, line 193) and each process row expands on click (lines 165–190), which mitigates length, but the sections themselves have no collapsed state: a card carrying gauges + six metric rows + two sparklines + process table + model list is the longest card in the app. The process detail is already an expandable; the sections should be too, with a visual summary standing in for them (see redesign).

### 2. One section, multiple views — Fail
The gauge pair (lines 209–212) and the metric rows directly beneath it (lines 213–219) are two stacked representations of the same data in one card — utilization as a ring and again as a row, VRAM as a ring and again as a row — rather than one representation chosen per datum.

### 3. Prime real estate — Pass
After the identity header, the first content element is the gauge pair (lines 209–212): the card leads with a visual, not prose or stat tiles about a visual.

### 4. Narrative in tooltips — Fail
Three narrative strings render inline:
- Peak-temperature window explanation appended to the Temperature value: `· peak ${temp_peak_c} °C since last reset or app start` (`static.ts:142`, rendered line 216).
- The down-GPU paragraph — two full sentences about last-known values and the fallen-off-the-bus reboot remedy (`static.ts:146`).
- The process-section instruction `PROCESSES (n) — click a row for detail` (`static.ts:193`).

The narrative that does exist inside the process-row expandable (lines 184–189, e.g. the under-1 GiB context explanation) is compliant — narrative in a collapsed expandable is where principle 5 allows it.

### 5. Real data — Pass
`gaugeRing`'s arc is proportional to the clamped value (`ui/src/charts.ts:48`); the VRAM ring's fill is `used/total` (`static.ts:135`); sparklines plot the actual sample series normalized to its own min/max (`charts.ts` sparkline). No decorative marks. Caveat carried into Q11: on a down GPU the gauges keep rendering last-known values at full strength, qualified only by the red note.

### 6. Numbers in one place — Fail
Within the card, two numbers each appear twice:
- Utilization: ring center (line 210) and the `UTILIZATION` metric row (line 214).
- VRAM used: ring center (line 211) and the `VRAM` metric row (line 215).

Related view-level duplication: the GPUs overview strip above the cards (`paintBody`, `static.ts:296–304`) repeats Util, VRAM, Temp, and Power for the same GPU a third time. The overview is a separate element from this card, but a redesign should settle which of the two owns each number.

### 7. Density with order — Fail (minor)
Sections run header → gauges → metrics → processes → models. "Models on this GPU" is the placement answer for this view (which models are resident here, at what share) yet it sits below the per-process detail table (`static.ts:200–204` after 192–194) — detail before summary for that dataset.

### 8. Multi-number data is a chart — Fail
- Resident-model rows render two related numbers as text: configured share % and GiB held, e.g. `via coder · 50% of the model · 13.8 GiB held` (`static.ts:200–201`).
- Per-process VRAM is a set of related quantities (each process's share of the device) rendered as a text table (lines 165–194). This is the canonical stacked-bar dataset: segments per process summing to VRAM used.
- Temperature and power histories do get sparklines (lines 151–154) — compliant there.

### 9. Label: Value — Fail
The metric labels themselves are compliant nouns (`UTILIZATION`, `VRAM`, `Temperature`, `Power`, lines 214–218) and the process-detail lines are Label: Value form (lines 185–188). Failures are the Q4 items in sentence form: the peak suffix (line 142), the down paragraph (line 146), and the `— click a row for detail` instruction (line 193). The model row `via ${backend} · ${pct}% of the model` (line 201) is a prose fragment carrying values, not a label/value pair or a chart key.

### 10. Copy buttons — Pass
The card shows no URL, key, or network address. The only address-like value is the PCI bus id in the header (line 137), which is identification text; a copy button there is optional, not required by the principle as scoped.

### 11. Status colors — Fail (minor)
- `stale` (degraded telemetry) renders amber via `badge-warn` (line 144) and the header dot maps `degraded` to `warn` (line 39). Principle 11 reserves amber for transitional states (starting/stopping); stale is a caution state, not a transition. The usage is consistent app-wide, so this is a semantics stretch to resolve deliberately, not a one-off bug.
- A down GPU gets the correct light-red chip (line 145) and red dot, but its gauges stay in live blue/purple at full opacity (lines 209–212) — the card's dominant visuals do not reflect the failed state; only the red paragraph does, and that paragraph is itself a Q4 violation.
- Compliant parts: the `context only` chip is uncolored/neutral (line 173) and is a measurement qualifier, not a state — correctly not forced into the status palette.

### 12. Unknown is not zero — Pass, with one minor edge
This is the card's strongest area, and the redesign must preserve it exactly:
- Null `vram_mib` renders `not measured` in the row (line 183), never a number.
- The `context only` qualifier is withheld for null (line 173: `p.vram_mib !== null && p.vram_mib < 1024`) — an unmeasured process never earns a label that implies a measurement.
- The expanded detail states `not measured — per-process accounting is unavailable for this GPU` (line 186).
- Resident-model rows render `VRAM held not measured` when `mib` is null (line 201).
- Power renders `N/A` when null (line 217) — an explicit marker, not 0. Fan and memory-util rows are omitted entirely when undefined (lines 147–150).

Minor edge: when `vram_total_gb` is 0 (total unknown), line 135 defaults the percentage to 0 and the card renders a `0 / 0 GiB` VRAM row and an empty ring — an unknown presented as a zero reading. The total-unknown case should omit the VRAM visual and value instead.

### 13. Empty states — Pass
Processes section omitted when no processes (lines 192–194 conditional), models section omitted when no rows and no sample models (lines 200–204), fan/memory-util rows omitted when undefined (lines 147–150). No placeholder prose anywhere in the card. (The down-GPU note is a state explanation, scored under Q4/Q9, not an empty state.)

### 14. Number formatting — Fail (minor)
`fmtGb`/`trim1` (`charts.ts:131–141`) implement the rule, and every GiB figure goes through them. The exceptions interpolate raw numbers:
- Power: `${g.power_w} W` and `${g.power_limit_w} W limit` (lines 143, 217). `power_w` is a float in the payload (`ui/src/types.ts:44`); nvidia-smi power draw carries two decimals, so the card can render `182.43 W` — over one decimal.
- Peak temperature: `${g.temp_peak_c}` raw (line 142).
- Per-process VRAM: `${p.vram_mib} MiB` (lines 183, 187) prints five-digit figures with no thousands separator (e.g. `11585 MiB`).

## Proposed redesign

Keep the header, the payload, and every Q12 behavior. Rebuild the body as: identity header → one VRAM residency bar (the card's prime visual) → a tile row of single values → collapsed sections for processes and detail. This mirrors the approved backend-card pattern (metric tiles, residency bar, one Details expandable).

### Layout (top to bottom)
1. **Header** (unchanged in content): status dot, GPU name, PCI id, driver version. `stale` chip stays but moves to the gray/neutral treatment unless the app deliberately redefines amber to include caution (decide once, app-wide). Down state: red chip stays, and the whole body below renders in the de-emphasized treatment (see below).
2. **VRAM residency stacked bar** — the prime visual, replacing both the VRAM gauge ring and the text process/model VRAM figures. One horizontal bar per card, width = VRAM used share of total:
   - One segment per resident backend process, width ∝ its measured `vram_mib`; segment key = backend label below the bar.
   - A process holding < 1 GiB renders as a thin sliver segment — the "context only" fact encoded by size, with the explanation in the segment's tooltip (the existing sentence from line 188).
   - Unattributed remainder (used − Σ processes) as a hatched/neutral segment keyed `other`.
   - Free space = the unfilled track.
   - **WDDM rule (principle 12):** when any resident process is unmeasured (`vram_mib === null`), no segment is drawn from invented data: the bar shows total used as a single neutral segment keyed `VRAM used — per-process breakdown not measured`, and the models list (below) carries presence from the configured split with `VRAM held not measured`. A measured zero is impossible to confuse with this, because measured segments always carry a GiB figure in their key.
   - The bar's right end carries the single VRAM number for the card: `VRAM: 13.8 / 16 GiB` (Label: Value, `fmtGb`).
3. **Utilization** — keep exactly one representation: a slim meter or the gauge ring, but not the ring plus a row. Recommendation: keep the gauge ring for utilization only (its center number is the value), and delete the `UTILIZATION` metric row.
4. **Tile row** — single labeled values, each rendered once: `Temp: 62 °C` (peak moves to the tile's tooltip: `Peak 81 °C since last reset or app start`), `Power: 182 W` (limit moves to tooltip: `Limit 250 W`; value passed through `trim1`), `Memory util: 38%` and `Fan: 45%` as tiles when present, omitted when undefined (as today). Temperature and power sparklines stay, attached under their tiles — they encode the history series.
5. **Models on this GPU** — moves above Processes. Each model becomes a labeled line whose share is a mini segmented bar (width ∝ configured `pct`), with `GiB held` as the value at the right when measured, or the text `not measured` (no figure) when null: `Model: <name> · via <backend>` + share bar + held value. No `· 50% of the model ·` prose; the percentage is the bar plus a `Share: 50%` value.
6. **Processes** — collapsed expandable (`Processes (n)` header, no instruction text; the expand affordance is the instruction). Inside, today's rows and per-row detail expandables are unchanged in content — they are already Label: Value with narrative correctly inside the detail expandable. MiB figures get thousands separators.
7. **Down GPU treatment** — body visuals render de-emphasized (reduced opacity / gray) with the values suffixed by a single `last known` key on the VRAM line; the two-sentence explanation moves to the down chip's tooltip. Readings stay visible (they are real, labeled as last known) instead of being narrated.

### What is omitted when empty (principle 13)
- No processes → Processes expandable absent (as today).
- No resident models → Models section absent (as today).
- Fan / memory-util absent → tile absent (as today).
- Power null → tile absent instead of `N/A` (tightens today's behavior to the principle; `N/A` is the one placeholder the current card renders).
- VRAM total unknown (0) → VRAM bar and VRAM value omitted; a `VRAM: not measured` affordance in their place, never `0 / 0 GiB`.

### Numbers-in-one-place resolution
Each number gets exactly one home in the card: utilization in the ring, VRAM used/total on the bar line, per-process VRAM in the bar segments (and repeated only inside the collapsed process detail, which is detail-on-demand, not a second visible rendering), temperature/power in their tiles, model share in the model bar. The view-level overview strip duplication (Q6 note) should be resolved in the same pass: either the overview keeps Util/VRAM/Temp/Power and the cards drop the tiles, or the overview becomes navigation-only (status + name). Recommendation: overview keeps the numbers (it's the cross-GPU comparison surface), cards keep the residency bar + tiles — accept the overview as the one sanctioned repetition, or drop Temp/Power from the overview. Flagged for the parent to decide; not scored against this card.

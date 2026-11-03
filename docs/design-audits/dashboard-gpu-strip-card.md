# Design audit — Dashboard GPU strip card

Card: `gpuStripCard`, `ui/src/views/dashboard.ts:123-148`. Renders in the Dashboard (Appliance view) GPUs group — the first group on the dashboard (`applianceView`, `dashboard.ts:226`), so view-level prime real estate is already correct. Audited against `docs/design-principles.md` Part 3, all 14 questions. Every answer must be "no"; failures carry code evidence.

**Verdict: minor gaps** — 7 of 14 checks fail, all minor. The card's bones are sound: its one real chart (the VRAM meter) encodes its value exactly, there is no narrative prose, no fabricated data, and absent data is omitted rather than placeholdered. The failures cluster on the VRAM row (duplicated number, inverted label/value), unkeyed sparklines, two text-packed number pairs, raw number formatting, and one off-vocabulary unknown token.

## Scorecard

| # | Check | Result | Evidence |
|---|---|---|---|
| 1 | Compact | Pass | Fixed strip; optional rows render only when their data exists (`dashboard.ts:129-137`). |
| 2 | One section, multiple views | Pass | No datum is presented two ways as stacked blocks inside the card (the VRAM text/meter pairing is scored under #6, not here). |
| 3 | Prime real estate | **Fail — minor** | The lead data element is a text row, `Utilization — N%` (`dashboard.ts:139`). The card's only true visual, the VRAM meter, is third (`dashboard.ts:142`), behind the utilization row and an optional sparkline. GPU RAM is this app's scarce resource; the VRAM share visual should lead the card. |
| 4 | Narrative in tooltips | Pass | No prose sentences inline. Values and fragments only. |
| 5 | Real data | **Fail — minor** | The meter fill is exact (`charts.ts:81-87`: clamped, linear in pct) and sparkline marks map to series values (`charts.ts:22-36`). But the three sparklines render with an empty label cell — `<span></span>` (`dashboard.ts:130,132,134`). A chart with no key: which metric a sparkline belongs to is inferable only from row position and color, and the utilization blue is reused for unrelated meters elsewhere. Principle 5 requires chart keys as visible text. |
| 6 | Numbers in one place | **Fail — minor** | `vramPct` is shown as text (`${vramPct}%`, `dashboard.ts:141`) and again as the meter fill (`dashboard.ts:142`) — the same number as both value and chart. Contrast with the approved backend-card pattern, where the bar's companion text is GiB, a different number. |
| 7 | Density with order | Pass | Rows group by metric, each sparkline directly under its metric. The lead-order complaint is scored under #3. |
| 8 | Multi-number data is a chart | **Fail — minor** | Two number pairs render as text: Temperature packs current + peak (`${g.temp_c} °C · peak ${g.temp_peak_c}`, `dashboard.ts:144`) with no visual for the peak even when a temperature series exists; Power packs draw + limit (`${g.power_w}${limit} W`, `dashboard.ts:146`) — a ratio (draw ÷ limit share) of exactly the kind the same card already charts for VRAM, rendered as a text fraction. |
| 9 | Label: Value | **Fail — minor** | The VRAM row inverts the structure: its label carries the values (`VRAM ${fmtGb(used)}/${fmtGb(total)} GB`, `dashboard.ts:141`) while the value cell holds the derived pct. Temperature's value cell smuggles a second labeled value ("· peak N", `dashboard.ts:144`); Power's value is an unlabeled ratio ("131 / 250 W"). The other labels (Utilization, Temperature, Power) are proper nouns. |
| 10 | Copy buttons | Pass | No URL, key, or address is rendered on this card (the type carries `pci?: string`, `types.ts:60`, but the card does not render it), so nothing lacks a copy button. |
| 11 | Status colors | Pass | `statusDot` (`dashboard.ts:58-61`): ok → green, degraded/starting → amber, down → red, else gray — principle 11's semantics exactly. Meter/sparkline colors are fixed per-metric identity colors, not status assertions; the VRAM meter is purple at any fill level, claiming nothing about state. |
| 12 | Unknown is not zero | **Fail — minor** | A null power reading renders `"N/A"` (`dashboard.ts:146`). It never renders 0 — but principle 12 sanctions exactly two renderings: an absent element or an explicit "not measured" affordance. "N/A" is a third token, inconsistent with the "not measured" vocabulary used across the app's telemetry surfaces, and a power sparkline (sample data) would still render beside it, implying a measurement the value cell disclaims. |
| 13 | Empty states | Pass | The card renders only for a GPU present in the payload, and absent series/peak/limit fragments are omitted (`dashboard.ts:129-137`). No placeholder prose anywhere in the card. |
| 14 | Number formatting | **Fail — minor** | VRAM figures pass through `fmtGb` = `trim1` (`charts.ts:132-142`) and comply. But `util_pct` (`dashboard.ts:139`), `temp_c` and `temp_peak_c` (`dashboard.ts:144`), and `power_w` / `power_limit_w` (`dashboard.ts:146`) are interpolated raw, and the API layer passes them through unrounded (`ui/src/api.ts:184-189`). NVML power draw is inherently fractional — a value like 131.37 renders verbatim as "131.37 W", over the one-decimal limit. The card has no formatting guard; compliance currently depends on the source happening to send integers. |

Gap count: **7** (checks 3, 5, 6, 8, 9, 12, 14 — all minor).

## Redesign proposal

Keep the strip's shape (one card per GPU, header with status dot + name) and rebuild the body around one lead visual:

1. **Lead visual — attributed VRAM bar (fixes #3, #6, #9).** Directly under the header, a full-width stacked bar of VRAM: one segment per resident process from `GpuInfo.processes` (already in the payload, `types.ts:65-68`), each segment's width exactly its measured share of total VRAM, free VRAM as the track. The bar answers the strip's real question — what is this GPU's memory, and who holds it. Companion value in Label: Value form: `VRAM: 13.8/16 GB`. The percentage appears exactly once — as the bar-end value — and the used/total figures move out of the label into the value cell. Segment identities are the bar's `title` tooltips plus the existing placement palette, not a text list.
2. **Utilization as a labeled value (supports #3).** `Utilization: 37%` as a single-value tile beside/under the bar. No second meter for it — a fill would duplicate the printed percentage (the #6 defect being removed from VRAM).
3. **Temperature as one value, peak to tooltip (fixes #8, #9).** `Temperature: 63 °C`; the peak moves to the row's tooltip (`Peak: 81 °C`).
4. **Power as a share meter (fixes #8, #9).** When both draw and limit exist, a thin meter whose fill is draw ÷ limit — a derived share printed nowhere as text — with value `Power: 131/250 W`. When only draw exists, the value alone, no meter. This mirrors the VRAM pattern: fill encodes the share, text carries the raw numbers, no number appears twice.
5. **Trends in one collapsed expandable, keyed (fixes #5).** The three sparklines move into a single collapsed "Trends" expandable; inside, each sparkline is labeled with its metric name (Utilization / Temperature / Power) as its chart key. The closed card carries no sparklines. (In the live payload the series are absent — `dashboard.ts:125-127` — so the expandable itself is omitted live, per principle 13.)
6. **Unmeasured handling (fixes #12).** `power_w == null` renders the value `not measured` — the app's established token — never "N/A" and never 0, and no power sparkline renders beside it. Processes whose `vram_mib` is null are excluded from the stacked bar (a zero-width segment would assert 0) and are named in the bar's tooltip as "not measured".
7. **Formatting guard (fixes #14).** Route `util_pct`, `temp_c`, `temp_peak_c`, `power_w`, and `power_limit_w` through `trim1` at render, so the card complies regardless of source precision.

Unchanged: header/status dot (check #11 passes as-is), group position, and the omission behavior for absent data (#13).

Robustness note (not scored): `vramPct` divides by `vram_total_gb` unguarded (`dashboard.ts:124`); a reported total of 0 would render `NaN%` and a NaN-width meter. The redesign's bar should treat a non-positive total as unmeasured under principle 12.

# Design Audit — Add-a-Backend Card

Scope: the transient add-a-backend form and its post-submit confirmation card, rendered inline in `wireBackends` (`ui/src/views/static.ts`, form at lines 1656–1687, confirmation at lines 1713–1716), Backends view. Audited against the 14-question checklist in `docs/design-principles.md` Part 3. Every answer must be "no"; failures carry code evidence. Audit only — no code was changed.

## Scorecard

| # | Checklist question (short) | Verdict | Evidence |
|---|---|---|---|
| 1 | Compact — anything that belongs behind a toggle/expandable? | Pass | The card itself is the compact control: it renders only when toggled open into `#backAddForm` and closes on re-toggle (`static.ts:1656–1658`); the confirmation replaces it in the same host (1713). |
| 2 | One section, multiple views — same data two ways, stacked? | Pass | Name/model/port/group are each presented exactly once; the confirmation does not restack the form. |
| 3 | Prime real estate — top element is the visual/value that answers the card's question? | **Fail (minor)** | The card's question is "what will be created, and will it fit?" Its top element is a title text block (`static.ts:1662`, "Add a backend") followed by raw inputs. No summary value or visual of the outcome exists anywhere in the card, let alone at the top. The confirmation's top is likewise a title (1713); the created values are buried in a prose sentence below it (1714). |
| 4 | Narrative in tooltips — any explanation/methodology inline? | **Fail (major)** | Three inline methodology blocks: (a) the footer paragraph, three sentences covering creation defaults and group traffic semantics (`static.ts:1679–1681`); (b) the Enabled checkbox label carrying its own explanation, "the gateway routes enabled backends; it can be started right away" (1676); (c) the confirmation's second sentence explaining the activation plan (1714). None is behind a tooltip or expandable. |
| 5 | Real data — any visual whose marks don't correspond to a value? | Pass | The card has no bars, fills, or gauges, so none can lie. Its one stated default ("32K context", 1680) matches the seeded backend defaults. |
| 6 | Numbers in one place — any number shown twice? | Pass | "32K" appears once (1680); the port appears once, as the input's computed default (1670, from `nextFreePort`, 1648–1655). The confirmation repeats the name/group the operator just entered, which is those values' only appearance in that state. |
| 7 | Density with order — grouped by question; summary before detail? | **Fail (minor)** | Field grouping itself is sound (identity, then routing, then behavior), but the explanatory prose is placed *after* the action buttons (`static.ts:1677–1681`: actions, then narrative, then the message line at 1682), so the operator meets "Add backend" before the text that explains what it does. The confirmation's summary is also incomplete as a summary: it names backend and group but omits the model, port, and enabled state just chosen (1714). |
| 8 | Multi-number data is a chart — related numbers rendered as text? | **Fail (major)** | The creation consequences are multi-number data — model size, GPU/System RAM requirement, and the per-GPU tensor split that will be assigned — and the card conveys them with one prose clause: "a tensor split is assigned from live GPU telemetry when available" (1680). No split bar, no requirement figures, no fit indication appear anywhere in the card, although the Placement form in this same view renders exactly this data as a segmented bar with a projection table, and `ui/src/charts.ts` already provides the segmented-bar primitive. |
| 9 | Label: Value — any visible sentence; any label that is a sentence? | **Fail (major)** | Labels carrying sentences or rules instead of nouns: "Enabled (the gateway routes enabled backends; it can be started right away)" (1676); "New group name (defaults to the backend name)" (1673); "Name (lowercase, digits, hyphens)" (1664 — a format rule in the label). The confirmation body is two full sentences with the values bolded inside the prose (1714) instead of "Backend: …", "Group: …" rows. The footer paragraph (1679–1681) is prose, not values. |
| 10 | Copy buttons — any URL, key, or address without one? | Pass | The card displays no URL, key, or address. The port is an editable input, not a presented value. |
| 11 | Status colors — any break of green/gray/light-red/amber; any chip stating the default? | Pass | No status chips. Error feedback uses `var(--err)` (1703, 1708), consistent with the failure semantics. The Enabled checkbox is a control, not a state chip. |
| 12 | Unknown is not zero — any unmeasured value rendered as 0 or as measured? | Pass | The one prefilled value, the port, is a computed real next-free port (1648–1655), not a placeholder 0. No unmeasured figure is presented. |
| 13 | Empty states — placeholder prose instead of omission? | **Fail (minor)** | With zero models registered, the card still renders the full form — including an empty Model select — with the submit merely disabled (1677), and the state is communicated as an inline prose sentence, "No models are registered yet — add one on the Models page first." (1681), instead of the form being omitted in favor of a compact directive. |
| 14 | Number formatting — >1 decimal, trailing `.0`, decimals at ≥100? | Pass | The only rendered numbers are "32K" (1680) and the integer port. |

**Result: 6 gaps** — Q3 (minor), Q4 (major), Q7 (minor), Q8 (major), Q9 (major), Q13 (minor). Verdict: **redesign needed**. The card is functionally complete and honestly labeled, but it explains a consequential, numeric operation entirely in prose.

## Redesign proposal

Same host, same toggle behavior, same fields, same submit path (`addBackend` call and validation at `static.ts:1690–1716` unchanged). Only presentation changes.

### Form state

- **Header row:** server icon + "Add a backend" + one info-tip. The info-tip carries the group-semantics text verbatim from the current footer: what joining an existing group does vs. creating a new one. The defaults sentence moves to the same tooltip. Nothing else from the footer survives inline.
- **Fields (noun labels only):** Name, Model, Port, Gateway group — the existing 2×2 grid. The Name field's format rule ("lowercase, digits, hyphens") moves to a tooltip on the label; the placeholder keeps the example value. "New group name" loses its parenthetical; the defaults-to-backend-name behavior goes into the header info-tip.
- **Enabled:** checkbox labeled "Enabled" only; its explanation becomes the label's tooltip.
- **Model summary strip (new, appears once a model is selected):** Label: Value tiles for the values the payload already carries — Context: 32K (the creation default), Size, GPU RAM, System RAM — using the same requirement figures the backend cards show. A tile whose figure the payload does not carry is omitted, never zero-filled (Q12).
- **Split preview (new visual):** directly under the summary strip, the segmented bar from `ui/src/charts.ts` — the same primitive the Placement form uses — showing the split that will be assigned from live telemetry: one segment per GPU, segment width = that GPU's percentage of the split, keyed "GPU n · name — %". This is the chart form of the clause currently at line 1680 (Q8). When telemetry is unavailable, the bar is omitted entirely (Q13) and the info-tip already explains that a split is assigned when telemetry is available.
- **Actions row:** Add backend / Cancel, unchanged, now immediately after the preview — no prose between the actions and the card's message line. The error/validation line stays where it is (transient feedback, not card data).

### No-models state

Render a compact card instead of the form: "Models: none registered" as a Label: Value line plus an "Add a model" button that navigates to the Models view. No fields, no disabled submit, no prose sentence (Q13).

### Confirmation state

- **Header:** check icon + "Backend added", unchanged.
- **Values, not a sentence:** Label: Value rows (or tiles matching the backend card's Details pattern) — Backend: `name` · Model: `short name` · Port: `port` · Group: `group` · Enabled: `yes/no`. This also completes the summary the current card omits (Q7).
- **Activation line:** one compact row, "Activation plan: recorded", with an info-tip carrying the current sentence verbatim ("The change is recorded in the activation plan at the top of this page — the gateway routes the new backend after the plan is executed."). Done button unchanged.

### What each visual encodes

- Segmented split bar: segment widths are the assigned split percentages per GPU (real assignment data, same source the backend receives).
- Summary tiles: single values only — creation-default context, model size, GPU/System RAM requirement — each shown exactly once (Q6), at the top of the card's lower half where the decision is made (Q3).

# Design Audit — Add-a-Model Card

Card: **Add-a-model card** (Models view, "Add a model" form)
Source: inline in `renderModels`, `ui/src/views/static.ts:2104–2118`; submit handler in `wireModels`, `ui/src/views/static.ts:2347–2371`
Reference: `docs/design-principles.md` (14 principles + Part 3 checklist)
Method: every checklist question answered from the rendering code; "yes" to a checklist question is a failure (gap). Line numbers are `ui/src/views/static.ts` unless noted.

## Scorecard

| # | Checklist question | Verdict | Evidence |
|---|---|---|---|
| 1 | Compact — anything that belongs behind a toggle/expandable? | **FAIL** | Whole form always expanded at the foot of the Models view: 4 inputs + button, `static.ts:2104–2117`; the two optional fields (Quant, File name) are always open, `:2108–2114` |
| 2 | One section, multiple views — same data two ways, stacked? | Pass | A form; no datum is presented twice. File name is a derived *default* of the URL, offered as an override input, not a second rendering (`:2113–2114`) |
| 3 | Prime real estate — top element is the answer/visual? | **FAIL** (minor) | Top row pairs the required Model id with the *optional* Quant label in equal 1fr columns (`:2105–2109`); the substantive required input, the source URL, is third (`:2111–2112`) |
| 4 | Narrative in tooltips — explanation visible inline? | **FAIL** | A full methodology/consequence sentence is rendered inline, permanently: "The model is added enabled, with no pinned SHA-256 — its downloads run unverified until a hash is recorded in the configuration." (`:2116`). A second explanation is embedded in a label: "File name (optional — defaults to the URL's file name)" (`:2113`) |
| 5 | Real data — any visual not corresponding to a value? | Pass | The card has no bars, fills, gauges, or decorative visuals; nothing can misencode |
| 6 | Numbers in one place — any number shown twice? | Pass | The card renders no numbers |
| 7 | Density with order — grouping/summary order wrong? | Pass | Fields group as identity (id, quant) → source (URL, file name) → action → feedback; that order answers the form's question. (The permanently reserved empty message row is counted under #1, not here) |
| 8 | Multi-number data as chart — related numbers as text? | Pass | No multi-number data exists in this card; it is an input form |
| 9 | Label: Value — any sentence as a visible string, any label a sentence? | **FAIL** | The `:2116` warning is a prose sentence, not "Label: Value". The File name label (`:2113`) is a noun plus an explanatory clause, against principle 9's "labels are nouns, not sentences" |
| 10 | Copy buttons — URL/key/address without one? | Pass | The card displays no URL, key, or address; the URL field is an input the operator types or pastes into, not a rendered value |
| 11 | Status colors — semantics broken, or a chip stating the default? | Pass | No chips. Submit feedback colors are consistent: errors `var(--err)`, success neutral `var(--text2)` (`:2355–2366`) |
| 12 | Unknown is not zero — unmeasured rendered as 0? | Pass | No measured or unmeasured values are rendered |
| 13 | Empty states — placeholder prose when there is no data? | Pass | The feedback slot `#addModelMsg` (`:2117`) renders nothing until the handler writes to it — no placeholder prose. (It does reserve a margined empty row at all times; layout waste, counted under #1) |
| 14 | Number formatting — >1 decimal, trailing `.0`, decimals ≥100? | Pass | No numbers are rendered |

**Result: 4 gaps** — questions 1, 3, 4, 9. Questions 4 and 9 fail on the same element (the `:2116` sentence); questions 1 and 3 are layout-grade, not data-grade.

## The failures in detail

### F1 — Permanent inline narrative (Q4, Q9) — `static.ts:2116`

```html
<div style="font-size:12px;color:var(--text2);margin-top:6px">The model is added enabled, with no pinned SHA-256 — its downloads run unverified until a hash is recorded in the configuration.</div>
```

This is exactly the pattern the last two UI passes removed from the backend cards and the Gateway view: a consequence/methodology sentence sitting inline as permanent chrome, read on every visit whether or not the operator is about to add a model. It is also the card's only sentence-form string, so it fails Q9 as well. The content is worth keeping — an unverified-download caution is real information — but principle 5 puts it behind an info-tip. The codebase already has the mechanism: the `.info-tip` span with a `title` used throughout this file (e.g. `static.ts:1024`, `:1046`, `:2863`).

### F2 — Label carrying an explanation (Q4, Q9) — `static.ts:2113`

"File name (optional — defaults to the URL's file name)" packs three things into one label: the noun, the optionality, and the defaulting rule. The defaulting rule is explanation and belongs in a tooltip on the label; the optionality is better carried by grouping (see redesign) than by a parenthetical repeated on two labels (`:2108` "Quant label (optional)" has the same pattern in miniature).

### F3 — Always-expanded form, optional fields at full height (Q1) — `static.ts:2104–2117`

Adding a model is an occasional operation, but the card permanently occupies five rows at the foot of the Models view — including two fields most adds never touch. Principle 1: the optional fields belong behind a compact control. The reserved-but-empty `#addModelMsg` row (`:2117`, `margin-top:6px` on an empty div) adds to the permanent footprint for a message that exists only after a submit attempt.

### F4 — Optional field in prime real estate (Q3) — `static.ts:2105–2112`

The card's first row is a 1fr/1fr grid: Model id (required) and Quant label (optional) get equal width and the top positions. The source URL — required, and the input that defines the download — is pushed to the second row at full width. The form's visual priority is the reverse of the fields' importance.

## What is already compliant

- No invented visuals: the card resists decoration, so Q5/Q6/Q8/Q12/Q14 pass trivially and correctly — a form has no data to chart, and adding one would violate principle 6.
- No state chips, so nothing states a default (Q11) — contrast the backend card's chip discipline.
- Feedback is transient and correctly colored (handler, `:2355–2366`); the required-fields check ("Model id and source URL are required.") is feedback, not card data, and is out of scope for Q9.
- Placeholders ("my-model-7b", "Q4_K_M", "model.gguf") are input affordances, not prose.

## Proposed redesign

Presentation only; the four fields, the Add action, and the handler's behavior (validation, `modelAdd` arguments, feedback) are unchanged.

**Layout (top to bottom):**

1. **Header row** — "Add a model" heading, unchanged. On the same row, right-aligned: an info-tip carrying the verification sentence **verbatim** ("The model is added enabled, with no pinned SHA-256 — its downloads run unverified until a hash is recorded in the configuration."). This keeps the caution one hover from the Add button without rendering it as permanent text. (Alternative if a persistent affordance is wanted: a gray chip "SHA-256: not pinned" with the sentence as its tooltip — a state chip for a non-default state, consistent with principle 11. The info-tip is the smaller change; either satisfies Q4/Q9.)
2. **Primary row** — the two required inputs on the first row, weighted toward the URL: grid `1fr 2fr` with "Model id" and "Source URL". Labels are bare nouns; "(https)" is dropped from the URL label (the scheme requirement is enforced by the add path and the placeholder already shows a URL-shaped affordance) — or kept as part of the noun phrase if the audit's implementer prefers; it is not a sentence either way.
3. **Optional fields, collapsed** — "Quant" and "File name" move into a collapsed expandable titled "Optional fields" beneath the primary row (the app's existing details/toggle pattern). Collapsed by default, so the card's resting height is header + one row + actions. Inside it, labels are bare nouns ("Quant", "File name"); the File name label gets an info-tip: "Defaults to the URL's file name." The word "(optional)" disappears from both labels — the expandable's title states it once.
4. **Actions row** — the Add model button, unchanged, with the header's info-tip visually adjacent to this row if the header placement reads as detached.
5. **Feedback** — `#addModelMsg` renders only when the handler has a message (element hidden/omitted until first write); no reserved empty row. Error/success coloring unchanged.

**What becomes a visual:** nothing — deliberately. This card's content is four inputs and one action; there is no numeric or state data to encode, and principle 6 forbids decorative marks. The one state worth surfacing (unverified-by-default) is carried by the info-tip/chip in step 1, which is an affordance, not a chart.

**What becomes Label: Value:** form labels are the form analogue — bare nouns ("Model id", "Source URL", "Quant", "File name") with no clauses. No rendered values exist in this card to reformat.

**What moves to tooltip/expandable:** the verification sentence → info-tip (verbatim); the file-name defaulting rule → info-tip on its label (shortened to its content: "Defaults to the URL's file name."); the optional fields themselves → collapsed "Optional fields" expandable.

**What is omitted when empty:** the feedback row (absent until a submit attempt produces a message); the optional fields (collapsed, so absent from the resting layout). Nothing else in the card is conditional — it has no data blocks that can be empty.

**Expected effect on the scorecard:** Q1 pass (optional content behind a compact control; no reserved empty row), Q3 pass (required inputs hold the top row, URL widest), Q4 pass (no inline explanation), Q9 pass (no sentence-form strings; labels are nouns). All other answers unchanged.

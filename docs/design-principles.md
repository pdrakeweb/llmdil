# Design Principles — LLMdil UI

The durable reference for this app's UI. Every card and view is designed against these rules and audited with the checklist in Part 3.

## Part 1 — Principles

### Layout

1. **Compact over long.** Never extend page length when a tab, toggle, accordion, or other compact control can hold the content.
2. **One section, multiple views.** Related representations of the same data are tabs inside one section, not stacked sections.
3. **Prime real estate carries the answer as a visualization.** The top of a view is for the chart or visual that answers the question — not stat tiles or text blocks about the answer.
4. **Density with order.** Group content by the question it answers; summary first, detail on demand through compact controls.

### Data and narrative

5. **Data visible, narrative in tooltips.** Visible text is numbers, labels, and chart keys; explanation and methodology live behind a tooltip or info-tip, or in a collapsed expandable.
6. **Every visual element encodes real data.** A chart whose marks don't correspond to the values is worse than no chart — fix it or remove it.
7. **Numbers live in one place.** A value shown in a chart is not repeated in a text block beside it; shared assumptions live in one assumptions area, not restated per section.
8. **Multi-number data is a chart, not text.** Anything numeric that is not a single number is represented as a chart or graph where possible.

### Values and state

9. **Values render as "Label: Value".** Single values carry a short label; labels are nouns, not sentences ("GPU RAM:", not "GPU RAM required:"). No prose sentences inline in cards or panels.
10. **Important values get a copy button.** URLs, keys, and addresses carry a small copy-to-clipboard button beside the value.
11. **Status color semantics.** Green: enabled, running, adopted — healthy/live states. Gray: disabled and neutral states. Light red: stopped, failed. Amber: starting, stopping — transitional states. A state chip exists only when the state is non-default information: there is an "adopted" chip and no "not adopted" chip.
12. **Unknown is not zero.** Unmeasured data renders as an absent element or an explicit "not measured" affordance — never as 0, and never with a qualifier that implies a measurement.
13. **Empty states render nothing.** When a data block has no data, the block is omitted rather than showing placeholder prose.
14. **Number formatting.** At most one decimal place; a trailing `.0` is dropped; whole numbers at magnitude ≥100, with thousands separators where the value is a count.

### Controls

15. **Icons over words for safe actions.** For non-destructive actions that are not critical to operations, buttons with icons are preferred over buttons with words — the button is icon-only (copy, reveal). One carve-out: the standard workflow action of a surface — the action the user most likely wants to take next on it — keeps its words AND carries an icon even when it is non-destructive (the activation banner's Review); icon-only is for secondary safe actions. Destructive and operation-critical actions keep their words AND carry an icon (Restart Service, Apply Changes). An icon-only button always retains an accessible name (aria-label) and a tooltip carrying the action's name, per rule 5.
16. **Enable/disable is a state switch** (Pete, 2026-10-18). Anything that can be enabled or disabled is controlled by the shared two-position switch (`enableSwitch`, `ui/src/switch.ts`), never by an action button. The switch shows the entity's current STATE: enabled = green track, knob right, check glyph; disabled = gray track, knob left, slash glyph. (The buttons it replaced showed the ACTION a click would take — a slash on enabled entities, a check on disabled ones — and were misread as state.) Clicking the switch toggles and fires the same command the old control fired, with its confirmations and guards unchanged. It is a native button with `role="switch"` and `aria-checked`, so it is keyboard-operable and its state is announced. Form fields inside edit forms (checkboxes, selects) and segmented mode selectors are not this control and keep their own idioms.

## Part 2 — Card inventory

Every distinct card type in the UI, where it renders, and its source.

| Card | Renders in | Source |
|---|---|---|
| Backend card | Backends view, grouped under gateway-group sections | `backendCard`, `ui/src/views/static.ts` |
| Add-a-backend card | Backends view (transient form, then a confirmation card) | inline in `wireBackends`, `ui/src/views/static.ts` |
| Dashboard service card | Dashboard (Appliance view), Backends group; the first card represents the gateway | `serviceCard`, `ui/src/views/dashboard.ts` |
| Dashboard GPU strip card | Dashboard (Appliance view), GPUs group | `gpuStripCard`, `ui/src/views/dashboard.ts` |
| Backend placements card | Dashboard (Appliance view) | inline in `applianceView`, `ui/src/views/dashboard.ts` |
| Throughput chart card | Dashboard (Appliance view); only when a series exists | inline in `applianceView`, `ui/src/views/dashboard.ts` |
| Recent activity card | Dashboard (Appliance view); only when events exist | inline in `applianceView`, `ui/src/views/dashboard.ts` |
| GPU card | GPUs view | `gpuCard`, `ui/src/views/static.ts` |
| Model card | Models view | `modelCard`, `ui/src/views/static.ts` |
| Add-a-model card | Models view (form) | inline in `renderModels`, `ui/src/views/static.ts` |
| Gateway summary card | Gateway view (status, port, groups, routing counts) | inline in `gatewayPaint`, `ui/src/views/static.ts` |
| Client endpoint card | Gateway view (base/LAN URLs, master key, admin UI) | inline in `gatewayPaint`, `ui/src/views/static.ts` |
| OpenRouter fallback tier card | Gateway view | inline in `gatewayPaint`, `ui/src/views/static.ts` |
| Routing flow card | Gateway view (group → members → fallbacks) | inline in `gatewayPaint`, `ui/src/views/static.ts` |
| Gateway group card | Gateway view, one per model group | inline in `gatewayPaint`, `ui/src/views/static.ts` |
| Routing rules card | Gateway view | inline in `gatewayPaint`, `ui/src/views/static.ts` |
| Generated config card | Gateway view (LiteLLM YAML preview) | inline in `gatewayPaint`, `ui/src/views/static.ts` |
| Activation plan card | Activation Plan view; the same markup renders in the activation banner at the top of the main column | `activationPlanHtml`, `ui/src/views/static.ts` |
| Pending changes card | Activation Plan view (changes table) | inline in `wireActivationPlan`, `ui/src/views/static.ts` |
| Wizard step card | Setup Wizard view, one row per step | `wizardStepRow`, `ui/src/views/static.ts` |
| Settings cards | Settings view — tabbed panels: General, Updates & diagnostics, Notifications, MXC sandbox, Remote Access (Admin API + admin users), Supervision | inline in `renderSettings` / `wireSettings`, `ui/src/views/static.ts` |
| MCP server card | MCP Servers view, one per registered server | inline in `renderMcp` / `wireMcp`, `ui/src/views/mcp.ts` |
| Optimize sections | Optimize view — consultation / plan, measurements, experiments, candidate discovery are section blocks, not cards | inline in `wireOptimize`, `ui/src/views/optimize.ts` |
| Logs filter card | Logs view (search, scope, time window, sort, pagination bar) | inline in `renderLogs`, `ui/src/views/static.ts` |

Not cards: the Models overview strip (`modelOverviewHtml`) is a summary table; the Service Map and the dashboard Console/Topology views are full-view layouts; log entries are table rows. Shared chart primitives (segmented/stacked bars, gauges, line charts) live in `ui/src/charts.ts` and are components, not cards.

## Part 3 — Audit checklist

Apply to one card type. Every answer must be "no" unless noted.

1. **Compact** — Does the card hold anything that belongs behind a tab, toggle, or expandable instead of extending the page?
2. **One section, multiple views** — Does the card present the same data two ways as stacked blocks instead of views of one section?
3. **Prime real estate** — Is the card's top element anything other than the visual or value that answers its question? (Must be no.)
4. **Narrative in tooltips** — Is any explanation or methodology visible inline rather than behind a tooltip, info-tip, or collapsed expandable?
5. **Real data** — Does any bar, fill, gauge, segment, or color fail to correspond to an actual value?
6. **Numbers in one place** — Is any number shown twice in the card — in a chart and in text, or in a tile and a line?
7. **Density with order** — Is any content grouped by something other than the question it answers, or detail placed before summary?
8. **Multi-number data is a chart** — Is any set of related numbers rendered as text instead of a chart or bar?
9. **Label: Value** — Does any visible string read as a sentence rather than "Label: Value" or a chart key? Is any label a sentence instead of a noun?
10. **Copy buttons** — Does any URL, key, or address lack a copy-to-clipboard button beside it?
11. **Status colors** — Does any state color break the green/gray/light-red/amber semantics? Does any chip state the default rather than non-default information?
12. **Unknown is not zero** — Does any unmeasured value render as 0, or with a qualifier that implies it was measured?
13. **Empty states** — Does any block render placeholder prose when it has no data, instead of being omitted?
14. **Number formatting** — Does any number exceed one decimal, keep a trailing `.0`, or show decimals at magnitude ≥100?
15. **Button form** — Does any non-destructive, non-critical action other than the surface's standard workflow action (its most likely next action) carry a word label instead of an icon only, or any destructive, operation-critical, or standard-workflow action lack its words or its icon? Does any icon-only button lack an aria-label or a tooltip naming the action? (Must be no.)
16. **Enable/disable switch** — Is any enable/disable control anything other than the shared state switch (`enableSwitch`), or does any switch show the action a click will take instead of the entity's current state? (Must be no.)

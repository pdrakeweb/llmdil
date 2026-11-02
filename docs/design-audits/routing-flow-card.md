# Design Audit — Routing Flow Card

Card: Routing flow card (Gateway view, group → members → fallbacks)
Source: inline in `gatewayPaint`, `ui/src/views/static.ts` — card built at lines 2929–2933; helpers `memberHtml` (2816–2820), `memberExcluded` (2826–2831), `fallbackHtml` (2832–2843); `strategyNote` defined 2924–2927 and appended inside the card at 2933. View assembly order at 2970.
Audited against: `docs/design-principles.md` Parts 1 and 3.

**Verdict: redesign needed.** 7 of 14 checklist questions fail. The card is labelled a flow diagram but is typographic: no mark in it encodes any quantity, dead routing paths render at the same visual weight as live ones, and the same group→member→fallback data is rendered again in full directly below it.

## Scores

| # | Question | Result |
|---|---|---|
| 1 | Compact | Pass |
| 2 | One section, multiple views | **Fail** |
| 3 | Prime real estate | Pass |
| 4 | Narrative in tooltips | **Fail** |
| 5 | Real data | **Fail** |
| 6 | Numbers in one place | Pass (see note under #2) |
| 7 | Density with order | **Fail** |
| 8 | Multi-number data is a chart | **Fail** |
| 9 | Label: Value | **Fail** |
| 10 | Copy buttons | Pass (no URL/key/address in this card) |
| 11 | Status colors | **Fail** |
| 12 | Unknown is not zero | Pass |
| 13 | Empty states | Pass |
| 14 | Number formatting | Pass (card renders no numbers) |

## Failures, with evidence

### #2 — One section, multiple views: the same data is stacked twice

The flow card renders every group's members and fallbacks (`static.ts:2932`, via `memberHtml`/`fallbackHtml`). The gateway group cards immediately below render the same members and fallbacks again, with the same helpers (`static.ts:2946`, `2949`), plus the enabled state and basis line. Two stacked blocks present identical group→member→fallback data; per principle 2 they should be two views (tabs) of one Routing section — the flow as the summary visual, the group cards as the detail view — not both in full, one under the other. (This is also where the member-name duplication lives; checklist #6 is scored within-card and formally passes, but the duplication is real and is charged here.)

### #4 / #9 — Narrative inline; a visible string reads as a sentence

`strategyNote` (`static.ts:2925–2926`) renders as visible prose inside the card (appended at 2933):

> "LiteLLM applies ONE global routing strategy — the strategy chips on the groups are labels, not per-group settings. Effective strategy: **X**."

The first two clauses are methodology and belong in a tooltip; only "Effective strategy: X" is a value, and it is buried inside the sentence instead of standing as a Label: Value pair. This is the card's only prose, and it fails both the tooltip rule (principle 5) and the Label: Value rule (principle 9).

### #5 — Real data: dead paths render at full visual weight

The card is a visualization (per the inventory: "group routing flow chips/diagram"), so its marks must correspond to routing reality. Three do not:

- **Disabled groups.** A group whose members are all disabled is omitted from the generated gateway config entirely, yet in the flow its name chip renders undimmed in the standard `.chip` style and its full member list follows at normal weight; only a small gray "disabled" chip differs (`static.ts:2932`). The group card below dims the whole card for the same state (`static.ts:2937`, `opacity:.65`) — the flow does not, so the diagram's most prominent marks claim a presence the group does not have.
- **Excluded members.** An excluded member's name renders in full text color; exclusion exists only as an appended amber chip (`static.ts:2817–2820`, and `fallbackHtml` at 2833, 2840). The name — the mark — is unchanged.
- **Fallback connectors.** The dashed connector `fallback - - →` is emitted unconditionally whenever a group has fallbacks (`static.ts:2932`), including when the fallback target is itself excluded and the generated config drops the reference. A connector is drawn to a path that does not exist in the running configuration; a chip annotates the target, but the connector mark itself is unconditional.

### #7 — Density with order: summary after detail

The one answer-level fact in the card — the effective global strategy — is placed after every per-group row, as a footnote paragraph at the bottom of the card (`static.ts:2933`, defined 2924–2927). Summary belongs before detail (principle 4).

### #8 — Multi-number data is a chart: the "flow" encodes no quantities

Nothing in the card is sized, weighted, or proportioned. Membership is a comma-separated mono text list (`static.ts:2931–2932`). The quantitative routing data exists in the same function's scope and is unused here:

- Per-backend request counts for the last hour (`requests_1h`, aggregated for the summary card's stats tiles at `static.ts:2857–2866`) — the actual traffic each member carries, the one quantity a routing *flow* exists to show.
- Per-group enabled fractions (`g.enabled_members` of `g.members.length`), which the group card below is reduced to printing as a text sentence (`static.ts:2942–2943`).

A flow whose edges/segments encode neither is a list with arrows. The shared primitives to build it already exist (`stackedBar`, `meter` in `ui/src/charts.ts:62,81`; node/edge styling `.map-node`/`.map-edge` in `ui/src/styles.css:389–396`).

### #11 — Status colors: amber used for a settled state

All three exclusion chips use `badge-warn` (`static.ts:2818`, `2833`, `2840`), which colors text `var(--warn)` = `#d29922`, amber (`ui/src/styles.css:15,384`). Principle 11 reserves amber for transitional states (starting, stopping). An excluded member is not transitional — it is settled: dropped when the config is generated. The card's own disabled-group chip uses the gray `status-off` (`static.ts:2932`; `ui/src/styles.css:205`), so the card applies two different color semantics to two flavors of the same "not in the generated config" fact. Exclusion should be gray like disabled, with the reason in the chip's tooltip.

## Passes worth noting

- #12: when the backend list fails to load, a banner states exclusion states may be incomplete rather than silently rendering members as included (`static.ts:2845–2851`); unknown member names are treated as active but are genuinely ambiguous (external endpoints), and no value renders as a fake 0.
- #13: with no groups configured the card is omitted entirely (`static.ts:2929`); a memberless group renders "—", an absence glyph, not placeholder prose.
- #3: the card's own top element is the flow itself, under its section title.

## Proposed redesign

**Structure — one Routing section, two tabbed views (fixes #2, #7).** Merge the flow card and the gateway group cards into a single "Routing" section with a tab switch: **Flow** (the diagram, default) and **Groups** (the existing per-group cards, unchanged as the detail view). At the top of the section, above the tabs, one Label: Value line: **Effective strategy: X**, with the "LiteLLM applies one global strategy; per-group strategy chips are labels" explanation as the info-tip on that label (fixes #4, #9, #7).

**The Flow tab — a real diagram (fixes #5, #8).** Three columns per group row — Group node → Member nodes → Fallback nodes — reusing the Service Map's `.map-node` / `.map-edge` styling (`ui/src/styles.css:389–396`):

- *Group node:* name as the label; below it the Label: Value sub-line "Members enabled: N of M" (from `enabled_members`/`members.length`). A disabled group renders the whole node and its edges at the same `.65` opacity the group card already uses, with the gray "disabled" chip — its visual weight now matches its contribution (zero) to the generated config.
- *Member edges/nodes:* each member node carries a `stackedBar` segment row per group (one bar per group, segments = members) where segment width is the member's share of the group's requests in the last hour (`requests_1h` from the backend rows already loaded in this function). Segment `<title>`: "name — N req (1h)". When no metrics window exists (`requests_1h` undefined), segments render equal-width in neutral outline and the bar's info-tip states the basis: "Configured membership — no metrics window yet." Equal width then encodes configuration, not invented traffic.
- *Excluded members:* name dimmed, gray chip "excluded" beside it, reason ("backend disabled" / "tier disabled" / "no active members") in the chip's `title`. No full-color name, no amber (fixes #11).
- *Fallback edges:* dashed (`.map-edge` dashed variant). If the fallback target is excluded, draw no connector; render the target as a gray chip with the exclusion reason in its tooltip. A connector must never assert a path the generated config drops (fixes #5).
- *Memberless group:* node sub-line reads "Members: 0" (a real configured count, not an unmeasured value) with a tooltip "Omitted from the generated gateway config."

**What becomes Label: Value:** Effective strategy (section top); Members enabled per group (node sub-line); per-segment request counts live in segment tooltips (numbers stay in one place — the counts are not repeated as text beside the bar).

**What moves to tooltip:** the global-strategy methodology; all three exclusion reasons; the traffic-share basis note; the omitted-from-config explanation.

**What moves to the other tab:** per-group detail (basis line, member/fallback Label: Value rows, strategy chip) stays in the Groups tab exactly as the current group cards render it — the Flow tab no longer duplicates those lists as text.

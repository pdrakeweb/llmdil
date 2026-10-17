# Design Audit — Gateway Group Card

Card: Gateway group card (one per model group), Gateway view.
Source: inline in `gatewayPaint`, `ui/src/views/static.ts:2947-2960`; member/fallback rendering via the shared helpers `memberHtml` (`static.ts:2862-2868`), `memberExcluded` (`static.ts:2872-2876`), and `fallbackHtml` (`static.ts:2878-2890`) in the same function.
Reference: `docs/design-principles.md` (14 principles + Part 3 checklist).
Data available to the card: `GatewayGroup` (`ui/src/api.ts:1135-1147`) — `name`, `members[]`, `strategy`, `fallbacks[]`, derived `enabled`, derived `enabled_members` — plus the backends list and OpenRouter tier state already in scope in `gatewayPaint` (per-member state is derived client-side by `memberExcluded`).

**Verdict: redesign needed. 7 of 14 checklist items fail.**

The card's central datum — which members are live and which are dropped from the generated config — is delivered entirely as prose and comma-joined text. That is the exact failure mode principles 8 and 9 exist to prevent, and it is compounded by an explanatory sentence inline (principle 5) and an amber chip used for a non-transitional state (principle 11).

## Checklist results

| # | Item | Result | Evidence |
|---|---|---|---|
| 1 | Compact | Pass | Card content is the group's own definition (members, fallbacks); nothing in it belongs behind a tab or expandable at real group sizes (1–3 members). Rendered two-across in `.grid2` (`static.ts:2948`). |
| 2 | One section, multiple views | Pass | Within the card, each datum renders once. (Observation only, scored in the routing flow card's audit: the same member list also renders in the Routing flow card, `static.ts:2943`.) |
| 3 | Prime real estate | **Fail** | The card's question is "is this group live in the generated config, and what serves it?" The top element is a heading row of chips — name, status, strategy (`static.ts:2949-2951`) — and the actual answer arrives below it as a prose sentence (`static.ts:2953-2954`). No visual or leading value carries the answer. |
| 4 | Narrative in tooltips | **Fail** | `static.ts:2954`: "No enabled backends — omitted from the generated gateway config" is a full explanatory sentence rendered inline on every disabled group card. It is consequence/methodology text and belongs in the tooltip of the disabled chip. |
| 5 | Real data | **Fail** (minor) | `static.ts:2949`: the group-name chip carries `border-color:var(--accent)` on every group regardless of any value — a color that encodes nothing. (The `opacity:.65` dimming of disabled cards at `static.ts:2948` does encode `enabled === false` and is legitimate.) |
| 6 | Numbers in one place | Pass | The counts in the basis line (`static.ts:2953`) appear once as numbers; the member list below enumerates names, not counts. Note for the redesign: once enablement becomes a bar, the "N of M" text must go — keeping both would create the duplication this item prohibits. |
| 7 | Density with order | Pass | Order is summary-then-detail and grouped by the card's question: header (identity, status, strategy) → basis → Members → Fallbacks. |
| 8 | Multi-number data is a chart | **Fail** (major) | Member enablement is a set of related per-member states plus a fraction (`enabled_members` of `members.length`). Both render as text: the fraction as a prose clause (`static.ts:2953`) and the per-member states as a comma-joined monospace string (`static.ts:2956`, fallbacks likewise at `static.ts:2958`). No chart, bar, or segment anywhere in the card. |
| 9 | Label: Value | **Fail** (major) | `static.ts:2953` — "2 of 3 member backends enabled" is a sentence, not "Label: Value". `static.ts:2954` — "No enabled backends — omitted from the generated gateway config" is a sentence. "Members" / "Fallbacks" (`static.ts:2955`, `2957`) are bare headings over running text rather than labeled values or chart keys. |
| 10 | Copy buttons | Pass | The card contains no URL, key, or address. Member names are internal backend ids, not copyable addresses. |
| 11 | Status colors | **Fail** (minor) | Two breaks. (a) Exclusion chips use `badge-warn` — `--warn: #d29922`, amber (`ui/src/styles.css:15`, `:384`) — emitted at `static.ts:2865` ("excluded — disabled"), `:2880` ("excluded — tier disabled"), `:2887` ("excluded — no active members"). Amber is reserved for transitional states (starting, stopping); an excluded member is a steady state, not a transition. (b) The "enabled" chip (`static.ts:2950`) states the default state on every healthy group; principle 11 admits a chip only for non-default information (there is an "adopted" chip and no "not adopted" chip). The "disabled" chip itself is correct: `status-off` gray (`ui/src/styles.css:205`) matches the disabled = gray mapping, as does `status-on` green (`:204`) as a color — the failure is the enabled chip's existence, not its hue. |
| 12 | Unknown is not zero | Pass | No unmeasured value renders as a number. When the backend list fails to load, exclusion states are not fabricated: the view prepends an explicit banner that exclusion states "may be incomplete" (`static.ts:2895-2897`) instead of silently rendering members as included. |
| 13 | Empty states | **Fail** (minor) | The Fallbacks block renders unconditionally: heading (`static.ts:2957`) plus the placeholder word "none" (`static.ts:2958`) on every group with no fallbacks. A block with no data should be omitted, not labeled. (The members "—" at `static.ts:2956` is the group's defining fact — a memberless group is why it is disabled — and is acceptable as a value.) |
| 14 | Number formatting | Pass | The only numbers rendered are integer counts (`enabled_members`, `members.length`); no decimals anywhere in the card. |

**Gap count: 7** — items 3, 4, 5, 8, 9, 11, 13. Items 8 and 9 are the structural ones; 4, 11, 13 are localized fixes that the redesign absorbs.

## Proposed redesign

No new data is needed: every element below is built from `members[]`, `fallbacks[]`, `enabled`, and the existing `memberExcluded` derivation, all already in scope in `gatewayPaint`. The shared chart primitives in `ui/src/charts.ts` (`stackedBar`, `charts.ts:62`) cover the bar.

Layout, top to bottom:

1. **Header row.** Group name as plain heading text (drop the accent-bordered chip — its color encodes nothing, item 5). Strategy stays as a chip at the right (it is a chart-key-style label). Status chip **only when disabled**: gray `status-off` chip reading "disabled", carrying an info-tip with the sentence that is currently inline — "No enabled backends — omitted from the generated gateway config" (absorbs items 4 and 11b). Enabled groups get no status chip; their state is carried by the enablement bar below and the absence of card dimming. Disabled cards keep the `opacity:.65` treatment.
2. **Member enablement bar** (replaces `static.ts:2952-2956` wholesale — the basis sentence, the "Members" heading, and the mono text list). One segmented bar, one segment per member in declared order, each segment's width equal (segments encode membership and state, not a share — widths are uniform by member, so no segment misrepresents a value):
   - Segment color encodes per-member state from `memberExcluded`: green (`status-on` palette) = member reaches the generated config; gray (`status-off` palette) = excluded (disabled backend, disabled OpenRouter tier, or external member counted active stays green).
   - Below the bar, member names as chart keys in the same order, each key in its segment's color; an excluded member's key carries a tooltip "Disabled — dropped from the generated gateway config" instead of the amber `badge-warn` chip (absorbs item 11a).
   - The fraction lives in exactly one place: the bar's tooltip, "Enabled: 2 of 3". No "N of M" sentence remains (preserves item 6).
   - A group with zero members renders no bar; the disabled chip's tooltip already explains the state.
3. **Fallbacks row** (replaces `static.ts:2957-2958`). Rendered **only when `fallbacks.length > 0`** (absorbs item 13): a single labeled line, "Fallbacks:" followed by the fallback names as keys with the same state coloring as member keys. A fallback whose target group has no active members gets the gray treatment plus a tooltip "No active members — dropped from the generated gateway config", replacing the amber chip at `static.ts:2887`.

What each visual encodes: segment count = member count; segment/key color = that member's derived inclusion in the generated config (`memberExcluded`); card opacity = the group's derived `enabled`; the disabled chip's presence = the group is excluded from the generated config. Nothing decorative remains.

Shared-helper note: `memberHtml` and `fallbackHtml` are also used by the Routing flow card (`static.ts:2943`). The amber-to-gray exclusion treatment should land in those helpers (or their replacements) so both cards change together; the flow card's own audit may go further.

Resulting closed card: header row, one bar with keys, and — only when present — one fallbacks line. The basis sentence, both bare headings, both mono text blocks, and the amber chips are gone.

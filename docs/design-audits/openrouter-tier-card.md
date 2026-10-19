# Design Audit — OpenRouter Fallback Tier Card

Scope: the OpenRouter fallback tier card, rendered inline in `gatewayPaint`,
`ui/src/views/static.ts` (Gateway view). Data: `cfg.openrouter`
(`GatewayOpenRouter`, `ui/src/api.ts:1176-1181` — `enabled`, `cred_ref`,
`daily_cap_usd`, `model_allowlist`) plus the probe value
`status.openrouter_cred_present` (`boolean | null`, `ui/src/api.ts:1259`).
Toggle wiring: `gwOrToggle`, `ui/src/views/static.ts:3240`.

Post-v0.1.40 note (2026-10-18): the tier's enable/disable control is no longer a toggle button — it is the shared two-position state switch (`enableSwitch`, `ui/src/switch.ts`; tracker #48, design rule 16), still carrying the `gwOrToggle` id, so the wiring this audit cites is unchanged. Passages below that describe an "Enable tier | Disable tier" button describe the pre-switch control.

Line numbers are as of this audit; the file is under active edit by another
change, so the block is best located by `const or = cfg.openrouter`
(currently `static.ts:2941`) and the `openrouter` template
(currently `static.ts:2959-2969`).

Method: the 14-question checklist in `docs/design-principles.md` Part 3,
applied to this card only. Every failure cites the rendering code.

## Scorecard

| # | Question | Result |
|---|---|---|
| 1 | Compact | **Fail (minor)** |
| 2 | One section, multiple views | Pass |
| 3 | Prime real estate | **Fail (minor)** |
| 4 | Narrative in tooltips | **Fail (major — the dominant defect)** |
| 5 | Real data | Pass |
| 6 | Numbers in one place | Pass |
| 7 | Density with order | **Fail (minor)** |
| 8 | Multi-number data is a chart | Pass (see data note) |
| 9 | Label: Value | **Fail (minor)** |
| 10 | Copy buttons | **Fail (minor)** |
| 11 | Status colors | **Fail (minor)** |
| 12 | Unknown is not zero | **Fail (minor)** |
| 13 | Empty states | **Fail** |
| 14 | Number formatting | **Fail** |

10 of 14 fail. Verdict: **redesign needed** — not because the card's data
layout is wrong (the four-row kv block is sound) but because state,
credential status, and every explanation are carried as prose instead of
chips, labeled values, and tooltips, and because the card renders when it
has nothing to show.

## Failures, with evidence

### Q4 — Narrative inline (major)

Six prose strings render inline; none is behind a tooltip or expandable,
in a view that already uses `.info-tip` for exactly this (the master-key
and admin-UI explanations in the client endpoint card):

- `static.ts:2954` — "Credential present in Windows Credential Manager."
- `static.ts:2956` — "Credential not found in Windows Credential Manager —
  the fallback cannot authenticate."
- `static.ts:2957` — pure methodology: "When the gateway starts, the key
  is read from Windows Credential Manager (reference above) and passed to
  LiteLLM as OPENROUTER_API_KEY. The key itself is never stored in the
  configuration."
- `static.ts:2966` — "No credential reference is set — the fallback cannot
  authenticate until one is configured."
- `static.ts:2967` — "The tier is disabled — fallback entries naming
  openrouter are dropped when the gateway config is generated, and no
  traffic is sent to OpenRouter."
- `static.ts:2961` — "The OpenRouter fallback tier is not configured."
  (also Q13).

### Q13 — Empty state renders placeholder prose

`static.ts:2960-2961`: when `cfg.openrouter` is `null`, the card still
renders in full — title plus the sentence "The OpenRouter fallback tier is
not configured." Per principle 13 the block should be omitted, the
pattern the dashboard's throughput and recent-activity cards follow
(rendered only when a series/events exist). Nothing else is lost by
omitting it: in the null state the card contains no data and no control
(the toggle at `static.ts:2968` only exists in the configured branch).

Not scored as failures: inside a configured card, "Credential reference:
not set" (`static.ts:2944`) and "Model allowlist: none"
(`static.ts:2946`) are configuration values, not empty blocks.

### Q14 — Number formatting

`static.ts:2945`: `` `$${or.daily_cap_usd.toFixed(2)} per day` `` —
`toFixed(2)` forces two decimals unconditionally: a $5 cap renders
"$5.00 per day" (trailing zeros kept) and $5.50 keeps two decimals where
principle 14 allows at most one. The same function already has a
compliant formatter, `trim1` (used for throughput at `static.ts:2903`).

### Q11 — State bypasses the color/chip system

- Enabled state renders as the text "yes"/"no" in a kv row
  (`static.ts:2943`), no chip, no color. The same view chips the identical
  state elsewhere: group cards use `chip status-tag status-on/status-off`
  (`static.ts:2985`), backend cards likewise (`static.ts:1206-1208`);
  the classes are defined at `ui/src/styles.css:204-207`.
- Credential failure (enabled, reference set, probe says not found,
  `static.ts:2956`; enabled, no reference, `static.ts:2966`) renders as
  red prose (`color:var(--err)`) instead of a `status-bad` chip.
- Credential success (`static.ts:2954`) renders as neutral prose instead
  of a `status-on` affordance.

No color is the *wrong* color and no chip states a default; the failure
is that this card's states opt out of the mandated semantics entirely.

### Q12 — Unknown presence has no affordance

When the probe returns `null` (presence unknown — `ui/src/api.ts:1259`
types it `boolean | null`), the card substitutes the standing methodology
paragraph (`static.ts:2957`). The reader cannot tell whether presence was
checked at all. Principle 12 requires unknown to render as an absent
element or an explicit "not measured"-style affordance; here it renders
as unrelated explanation. (It does not fabricate a value — no false
"present" or 0 — so this is minor.)

### Q10 — Credential reference has no copy button

`static.ts:2944` renders the credential reference as plain text. The
reference is the Windows Credential Manager target name — the address of
the secret, needed verbatim when creating or checking that credential —
and principle 10 covers "URLs, keys, and addresses". The sibling client
endpoint card in the same function gives Copy buttons to the base URL,
LAN URL, and master key (`gwBaseUrlCopyBtn`, `gwLanUrlCopyBtn`,
`gwKeyCopyBtn`).

### Q3 — Prime real estate

The card's question is "is the cloud fallback armed and usable?" That
answer is a composite — enabled **and** credential authenticating — but
no single element states it. The card leads with the kv list whose first
row is "Enabled: yes/no" (`static.ts:2943`); the credential half of the
answer exists only as a footer block below the rows (`credHtml`,
`static.ts:2952-2957`, rendered at `static.ts:2965`). The verdict the
card exists to give must be assembled by the reader from a row plus a
sentence.

### Q7 — Density with order

One question — the credential — is split across two places: the reference
is row 2 (`static.ts:2944`); its probed status is detached into the
footer prose (`static.ts:2952-2957`), and in the disabled branch the
presence block is appended after the disabled explanation
(`static.ts:2967`). State is also not where sibling cards put it:
backend and group cards carry state chips in the header
(`static.ts:1206-1208`, `static.ts:2985`); this card buries it as the
first data row and repeats it as the toggle button's label
(`static.ts:2968`).

### Q9 — Label: Value

The four kv rows are proper noun-label pairs (`static.ts:2943-2946`) and
pass. The failures are the standalone sentences scored under Q4 (visible
strings that are neither "Label: Value" nor chart keys) and the Daily
cap value itself: "$5.00 per day" (`static.ts:2945`) embeds a prose unit
phrase in the value; the period belongs in the label or a tooltip, the
value should be the amount.

### Q1 — Compact

Minor, and a consequence of Q4: the prose blocks (up to two stacked
`margin-top:8px` divs, `static.ts:2954-2957` and `static.ts:2966-2967`)
are what extends this card's height. With them in tooltips the card is
four rows plus an action row — compact. Nothing else in the card
warrants a tab or expandable.

## Passes, with evidence

- **Q2** — One representation only: a single kv block
  (`static.ts:2962`). The toggle label restating state is a control, not
  a second view.
- **Q5** — The card contains no bars, fills, gauges, or decorative color;
  nothing visual claims to encode data it doesn't.
- **Q6** — The daily cap appears once (`static.ts:2945`); the credential
  reference appears once (`static.ts:2944`) and is referred to, not
  repeated, by `credHtml` ("reference above", `static.ts:2957`).
- **Q8** — Data note: the card's only number is the daily cap, a single
  value, correctly a labeled value rather than a chart. No spend/usage
  figure exists in the payload (`GatewayOpenRouter`,
  `ui/src/api.ts:1176-1181`, has no usage field), so no cap-vs-spend bar
  is possible today — and none is proposed below, because inventing the
  spend side would violate principle 6.

## Proposed redesign

Layout (configured tier):

```
[icon] OpenRouter fallback tier            [chip: enabled | disabled]
Credential: my-openrouter-ref            [Copy]
Credential status: [chip: present | not found | not checked]   (i)
Daily cap: $5                                                  (i)
Model allowlist: 3 models  [▾ names as chips, collapsed]
                              [ Enable tier | Disable tier ]
```

- **Omit when unconfigured.** `cfg.openrouter === null` → the
  `openrouter` segment returns `""`; no card, no placeholder sentence.
- **State moves to the header as chips**, using the existing classes
  (`ui/src/styles.css:204-207`), matching the group cards
  (`static.ts:2985`):
  - Tier chip: `status-on` "enabled" / `status-off` "disabled". The
    "Enabled: yes/no" row is deleted (fixes Q11, Q7, and the
    row/button-label duplication).
  - Credential chip, beside a "Credential status:" label, present only
    when a reference is set: `status-on` "present" (probe true),
    `status-bad` "not found" (probe false), neutral chip "not checked"
    (probe null — the explicit unknown affordance Q12 requires).
  - Enabled with no reference: credential chip `status-bad`
    "no credential reference" in place of the status chip.
- **Rows stay the existing kv block**, minus the Enabled row:
  - "Credential: `<ref>`" with a small Copy button in the
    `gwKeyCopyBtn` pattern (fixes Q10). Row omitted when no reference is
    set — the chip carries that state.
  - "Daily cap: $5" — value formatted with the `trim1` rule (at most one
    decimal, trailing `.0` dropped; fixes Q14). The "per day" fact moves
    to an info-tip on the label: "Spending cap per day."
  - "Model allowlist: 3 models" with the names behind a collapsed
    expandable as chips; empty allowlist keeps the value "none".
- **All narrative becomes tooltips**, verbatim from the current strings:
  - Info-tip on the Credential label: the key-reading methodology
    (current `static.ts:2957` text, unchanged).
  - Info-tip on the disabled tier chip: the dropped-entries text
    (current `static.ts:2967`, unchanged).
  - Info-tip on the "not found" chip: "The fallback cannot
    authenticate."; on the "no credential reference" chip: "The fallback
    cannot authenticate until one is configured." (from
    `static.ts:2956`/`2966`).
  No sentence remains inline (fixes Q4, Q9, Q1).
- **Disabled presentation:** rows remain visible but the card body takes
  the muted treatment the Gateway view already applies to disabled group
  cards; the credential row and chip stay live, since presence decides
  whether re-enabling works. The disabled explanation is the chip tooltip
  above, not a paragraph, and `credHtml`'s separate footer disappears —
  presence is the row-level chip, next to the reference it describes
  (fixes Q7, Q3: header chip + credential chip together state "armed and
  usable" at the top of the card).
- **No chart is added.** With one numeric value and no usage data in the
  payload, a chart would be decoration. If a daily-spend probe is ever
  added to `GatewayStatusData`, the honest visual is a single
  spent-vs-cap bar in the metric-strip style the summary card uses for
  gateway stats (`static.ts:2900-2906`) — proposed here only as the
  designated slot, not built on data that doesn't exist.
- **Unchanged:** the toggle button and its wiring (`gwOrToggle`), the
  probe semantics (presence only, value never sent to the UI), and the
  card's position in the Gateway view.

## Bottom line

The kv core of this card is the right shape; everything around it —
state, credential status, explanations, the unconfigured case — predates
the design rules and is carried as prose. The redesign is a
re-presentation of data the card already has: chips for state, a chip
for credential presence (including an explicit unknown), a copy button
on the reference, P14 formatting on the cap, tooltips for all six prose
strings, and omission when the tier is not configured.

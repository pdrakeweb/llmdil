# Design Audit — Settings Cards

**Scope:** the `settingsCard` card type (`ui/src/views/static.ts:3322`) and its four instances in the Settings view — General, Updates & diagnostics, Notifications, MXC sandbox — assembled by `settingsPaint` (`ui/src/views/static.ts:3416`) and returned as one concatenated stack (`ui/src/views/static.ts:3515`).

**Method:** scored against the 14-question checklist in `docs/design-principles.md` Part 3, from the rendering code only. Line numbers are a single snapshot taken during the audit; `static.ts` is under concurrent edit, so lines may drift a few lines either way. Function names are the stable reference.

**Verdict: redesign needed — 11 of 14 checklist questions fail.**

## Scorecard

| # | Question | Result |
|---|---|---|
| 1 | Compact | **Fail** |
| 2 | One section, multiple views | **Fail** |
| 3 | Prime real estate | Pass |
| 4 | Narrative in tooltips | **Fail** |
| 5 | Real data | Pass |
| 6 | Numbers in one place | **Fail** |
| 7 | Density with order | **Fail** |
| 8 | Multi-number data is a chart | **Fail** |
| 9 | Label: Value | **Fail** |
| 10 | Copy buttons | **Fail** |
| 11 | Status colors | **Fail** |
| 12 | Unknown is not zero | Pass |
| 13 | Empty states | **Fail** |
| 14 | Number formatting | **Fail** |

## Findings

### 1. Compact — Fail

- The Settings view is four full cards stacked in a single 960px column (`renderSettings`, `ui/src/views/static.ts:3518`; assembly at `:3515`: `return general + updatesCard + notifCard + mxcCard`). Nothing is behind a tab, toggle, or accordion; the page is as long as the sum of all four cards.
- The MXC card truncates its rule lists instead of offering detail on demand: `capRows` caps filesystem, network, and credential rules at 12 rows each (`ui/src/views/static.ts:3333`, applied in `mxcPolicyInner`) and activity at 8 entries, and `moreNote` (`:3339`) renders the overflow as a static `+ N more` text line. There is no expand control — the elided rules are not hidden behind a compact control, they are unreachable. That is the worst form of the violation: principle 1 exists to keep detail *available* without length, not to delete it.

### 2. One section, multiple views — Fail

- Yes — the four stacked cards violate principle 2 at the view level. "Settings" is one section (the application's configuration); General, Updates & diagnostics, Notifications, and MXC sandbox are four views of it, rendered as four stacked sections (`:3446`, `:3481`, `:3485`, `:3512`, concatenated at `:3515`). Principle 2's prescribed form is tabs inside one section.
- Inside the MXC card the same pattern repeats one level down: the policy's rules — one kind of data — are three stacked lists (Filesystem rules `:3380`, Network rules `:3382`, Credentials `:3384`), plus a stacked Recent activity block (`:3413`), instead of views of one rules section.

### 3. Prime real estate — Pass

Each card leads with its title and its primary values (General leads with Data directory / Configuration file, `:3425`/`:3427`; the MXC card leads with Mode / Policy file). No card opens with a text block about its answer. (The MXC card's *actionable* control being buried is an ordering problem, scored under Q7.)

### 4. Narrative in tooltips — Fail

Explanations and methodology are visible inline in at least six places; the `info-tip` pattern used elsewhere in this file (e.g. backend cards) is never used in the settings cards:

- Updates card: "Update checking is not supported in v0.1.x." rendered as an inline line (`ui/src/views/static.ts:3476`).
- Updates card: "The bundle contains raw, unredacted process logs. Review it before sharing." as an inline footnote under the Export button (`:3480`) — a warning, but still narrative prose inline rather than on the button's tooltip/confirm.
- MXC mode control: three definitional sentences rendered permanently under the mode buttons — "Learning — observe and log only; nothing is blocked." etc. (`:3503`) — followed by a methodology paragraph: "The mode is saved configuration: this app records it, and the MXC sandbox runtime applies it when that runtime executes tools." (`:3507`).
- MXC policy: the provenance sentence is a visible line — "No policy file exists — the built-in default policy is in effect." / "Loaded from the configured policy file." (`mxcPolicyInner`, `:3361`, rendered at `:3377`).
- MXC policy basics: the explanation is embedded in a value — "deny — anything not explicitly allowed is denied" (`:3365`).
- MXC activity: each entry's `description` renders as a visible sub-line under the entry (`:3410`).
- MXC self-test result: a remediation sentence renders inline — "To fix a failing case, edit the policy file at the Policy file path shown above and restart the app to apply it." (`:3594`).

### 5. Real data — Pass

The settings cards contain no bars, fills, gauges, or segments, so nothing misrepresents a value. The one visual state — the active MXC mode button carrying the `primary` class — is bound to the actual configured mode (`modeLc === m.mode`, in `settingsPaint`). Absence of charts is scored under Q8, not here.

### 6. Numbers in one place — Fail

- The MXC mode is stated three times in one card: as a Label: Value row ("Mode", pushed at `ui/src/views/static.ts:3493`), as the highlighted button in the mode button group, and again as the bold lead word of each of the three always-visible description lines (`:3503`). The button group alone fully encodes the current mode; the row and the repeated names are duplication.
- Rule counts are stated in the subhead text — "Filesystem rules (N)", "Network rules (N)", "Credentials (N)" (`:3380`, `:3382`, `:3384`) — directly above the lists that show those same rows, and the truncation note then states a third derived count ("+ N more", `:3339`).

### 7. Density with order — Fail

- General card: one undifferentiated row list mixes four unrelated questions — file paths, a telemetry record dump, Windows ML state, setup state (`settingsPaint`, `:3424`–`:3445`). Worse, the telemetry record's raw subkeys are expanded into the summary list (up to 8 entries via `summarizeEntries`, `:3313`, pushed as rows at `:3434`): detail is placed in the summary instead of on demand.
- MXC card: the card's one actionable control — the mode buttons — renders *after* the policy basics, up to 36 rule rows, and the activity list (`mxcCard` assembly, `:3512`). Summary/action last, detail first: the ordering principle inverted.

### 8. Multi-number data is a chart — Fail

- Updates → Components: each component row packs three facts into one text string — `pinned <version> · auto-update on · checked <date>` (`ui/src/views/static.ts:3458`–`:3460`). A set of related per-component values (version, update state, last-checked) rendered as prose-joined text instead of labeled values or a small table.
- MXC activity: the entries carry `decision` and an aggregation count (`×N`, `:3407`–`:3410`), so an allowed-vs-denied segmented count bar (the `charts.ts` segmented-bar primitive already exists) is directly computable, yet the decision mix is presented only as a text list.

### 9. Label: Value — Fail

The `settingsRows` renderer itself is compliant — key left, value right, one pair per row (`ui/src/views/static.ts:3317`). The failures are in what is fed to it:

- Labels generated from raw config keys: `summarizeEntries` emits the config's own key names, so the General card shows labels like `Telemetry · poll_interval_ms` (`:3434`, raw key interpolated verbatim) and the Notifications card renders the notification config's raw keys as its labels (`summarizeEntries(notif)`, `:3486`). Snake-case storage keys are not the noun labels principle 9 requires.
- Values that are sentences or multi-fact strings: "deny — anything not explicitly allowed is denied" (`:3365`); the component strings under Q8 (`:3458`).
- Section subheads ("Components", "Active policy", "Recent activity") are fine — noted so the finding is scoped to generated labels and value strings, not the renderer.

### 10. Copy buttons — Fail

- "Data directory" (`ui/src/views/static.ts:3425`) and "Configuration file" (`:3427`) are filesystem addresses rendered as plain text. The MXC "Policy file" path (`:3495`) is the same. No settings card renders a copy button anywhere.
- The pattern exists in the same file and view family: the Gateway client-endpoint card renders Copy buttons beside its Base URL, LAN base URL, and master key (`:2913`–`:2928` region). Settings paths — the values a user most often needs to paste into a terminal or a bug report — are exactly the "addresses" principle 10 names.

### 11. Status colors — Fail

- MXC activity decisions render with the single neutral `.chip` style for every value: `<span class="chip">${esc(e.decision)}</span>` (`ui/src/views/static.ts:3407`). Decisions in this domain are allow / prompt / deny (network-rule decisions are typed as such at `ui/src/api.ts:1475`). A *deny* decision therefore renders in the same gray as an *allow*, breaking the green / light-red / amber semantics. The semantic text classes already exist and are unused here: `.badge-ok`, `.badge-warn`, `.badge-err` (`ui/src/styles.css:384`).
- Secondary: state values that elsewhere in the app are colored chips are bare text here — "Windows ML: enabled/disabled" (`:3441`), "UI access: enabled/disabled" (MXC basics), "Setup completed: yes/no" (`:3444`). Not a color *break*, but the states carry no color semantics at all.

### 12. Unknown is not zero — Pass

This is the card type's strongest area. `summarizeValue` renders an em dash for values it cannot represent rather than inventing a figure (`ui/src/views/static.ts:3304`); fetch failures render as "unavailable — \<error\>" and are explicitly kept distinct from genuine absence (`mxcPolicyInner` `:3351`–, `mxcActivityInner` `:3390`–); counts are real array lengths. No unmeasured value renders as 0.

### 13. Empty states — Fail

Blocks with no data render placeholder prose instead of being omitted, in five places:

- `settingsRows` with no rows renders "Nothing reported." (`ui/src/views/static.ts:3318`) — the shared renderer guarantees the violation for any empty section fed to it.
- Notifications with no settings: "No notification settings reported." (`:3487`).
- Updates with no policy: "No update policy reported." (`:3473`).
- MXC activity with no data: "No recent MXC activity reported." / "No MXC activity recorded yet." (`:3396`, `:3399`).
- MXC policy absent: a prose line in place of the policy block (`mxcPolicyInner`, `:3355` region).

The code itself shows the standard is known but applied inconsistently: the comment at `:3465`–`:3469` describes suppressing exactly this placeholder above the Components list — for that one call site only. (Fetch-*error* lines — "Policy unavailable — \<error\>" — are state, not empty data, and are not counted here.)

### 14. Number formatting — Fail

- Values pass through unformatted: `summarizeValue` returns primitives verbatim (`String(v)`, `:3304`–`:3310`), and `summarizeEntries` feeds arbitrary config values through it, so any multi-decimal number in the config renders at full precision — principle 14's one-decimal rule is unenforced on this path.
- "Telemetry poll interval" interpolates the raw number: `` `${tel[intervalKey]} ms` `` (`:3431`, also `:3437`) — no thousands separator at magnitude ≥100 (a 1000 ms interval renders "1000 ms"), and no use of the app's number formatting used elsewhere.

## Proposed redesign

One Settings section, four tabs — the card shell gains a tab bar and each current card becomes a tab panel. Everything below reuses existing renderers and primitives; no new data is needed.

**Shell (fixes Q1, Q2).** Replace the four stacked `settingsCard` divs with one card: a tab row (General · Updates & diagnostics · Notifications · MXC sandbox) over a single panel. The Notifications tab is rendered only when notification settings exist (Q13). Page length becomes the longest tab, not the sum of all four.

**General tab (fixes Q7, Q9, Q10, Q14).**
- Keep `settingsRows` (it is compliant) for the summary rows: Data directory, Configuration file, Telemetry poll interval, Windows ML, Setup completed.
- Copy buttons beside Data directory and Configuration file, using the Gateway endpoint card's existing Copy-button pattern.
- Humanize labels through a small key→label map for known config keys; format the poll interval with the app number formatter ("1,000 ms").
- The raw telemetry subkeys and any unmapped config entries move into a collapsed "All configuration values" expandable at the bottom of the tab — detail on demand instead of dumped into the summary.

**Updates & diagnostics tab (fixes Q4, Q8, Q13).**
- Components render as one row per component with three labeled mini-values — `Version: pinned 1.2.3 · Auto-update: on · Checked: Oct 3` — instead of one unlabeled joined string.
- "Update checking is not supported in v0.1.x." becomes an info-tip on the tab's Updates heading.
- The diagnostics warning moves to an info-tip beside the Export diagnostics bundle button (the warning text is preserved verbatim in the tip).
- No policy and no components → the policy block is omitted entirely (the diagnostics actions remain; they are the tab's content, not an empty state).

**Notifications tab (fixes Q9, Q13).** Same Label: Value rows with humanized labels via the same key→label map. When there are no notification settings, the tab is not rendered at all (see shell).

**MXC sandbox tab (fixes Q1, Q4, Q6, Q7, Q8, Q11, Q13).**
- Order inverted to summary-first: (1) Mode control, (2) Active policy, (3) Recent activity, (4) self-test.
- The "Mode" Label: Value row is deleted — the button group is the one place the mode lives (Q6). Each mode button carries its definition as its tooltip (`title`), and the "saved configuration / runtime applies it" methodology becomes one info-tip beside the Mode label (Q4).
- Policy provenance becomes an info-tip on the "Active policy" heading; "Default: deny" loses its inline gloss to an info-tip (Q4).
- The three rule lists become one "Policy rules" block with a sub-tab or segmented filter (Filesystem · Network · Credentials, each labeled with its count — counts live in the filter keys, not repeated above the rows). Each list is complete inside a collapsed expandable when long ("Show all N rules"), replacing `capRows`/`moreNote` truncation — every rule reachable, none extending the page by default (Q1, Q6).
- Activity decisions get semantic colors using the existing classes: allow → `.badge-ok`, deny → `.badge-err`, prompt → `.badge-warn` (Q11). Entry `description` moves to the entry's `title` tooltip (Q4). When entries exist, a one-line segmented bar of aggregated decision counts (allowed vs denied share, from the existing `×N` aggregation) sits above the list (Q8).
- Empty policy/activity states omit their blocks; fetch errors keep their distinct "unavailable — \<error\>" rendering (Q12 behavior preserved).

**Formatting (fixes Q14).** Route every numeric value in the settings painters through the app's shared number formatting (one decimal, trailing `.0` dropped, thousands separators at ≥100) — including values arriving via `summarizeValue`, which is the leak point.

## What already complies

- The `settingsRows` Label: Value renderer (Q9 mechanism) and the cards' lead-with-values structure (Q3).
- Unknown/error handling throughout (Q12) — the model the other card types should copy.
- No decorative or fake visuals (Q5).

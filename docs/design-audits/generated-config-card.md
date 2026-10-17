# Design Audit — Generated Config Card

- **Card:** Generated config card (LiteLLM YAML preview), Gateway view
- **Source:** inline in `gatewayPaint`, `ui/src/views/static.ts:2972–2979`; toggle wiring at `ui/src/views/static.ts:3072–3091`
- **Audited against:** `docs/design-principles.md` (14 principles + Part 3 checklist)
- **Date:** 2026-10-16
- **Verdict:** Redesign needed — 7 of 14 checklist questions fail, from 5 distinct defects. The card's skeleton (raw text behind a toggle, refetched on every open, rendered last in the view) is sound; its chrome — always-visible prose, no summary values, no copy path — is not.

## Scorecard

Every checklist answer must be "no". Failures carry code evidence.

| # | Question (short) | Result | Evidence |
|---|---|---|---|
| 1 | Compact | **Fail (minor)** | The YAML body is correctly behind the `gwYamlBtn` toggle (`static.ts:2974–2978`), but the two-sentence explainer at `static.ts:2973` is rendered unconditionally, so the collapsed card permanently costs a title line + a prose paragraph + a button row for content that is entirely on-demand. |
| 2 | One section, multiple views | Pass | The card renders only the raw YAML; it does not restate it in a second form. The structured views of the same config (group cards, routing flow) are separate cards, not blocks inside this one. |
| 3 | Prime real estate | **Fail** | Inside the card, the first content element is methodology prose (`static.ts:2973`), not the artifact or any value answering the card's question ("what will the gateway run, and does it match what's running?"). No summary value appears anywhere in the card. (View-level placement is correct: the card is last in the `gatewayPaint` return order, `static.ts:2981`.) |
| 4 | Narrative in tooltips | **Fail** | Two methodology blocks are inline. `static.ts:2973`: "The running gateway reads the config file written when it was last (re)started from the Setup Wizard. This preview is regenerated from the current configuration — after edits, the two can differ until the gateway is restarted." — visible even when the preview is closed. `static.ts:2978`: "`api_key: local` in the generated config is LiteLLM's required non-empty placeholder — it is not a real key." |
| 5 | Real data | Pass | The card has no bars, fills, gauges, or segments; nothing can mis-encode. |
| 6 | Numbers in one place | Pass | No number is duplicated within the card. The YAML text itself is the canonical artifact, not a repetition of an in-card chart or tile. |
| 7 | Density with order | **Fail** | There is no summary layer at all: title → methodology prose → (on toggle) the full file. Detail is the only layer, and prose precedes it. Nothing states the artifact's basic facts as values — line count, deployment/model count, or that it was generated from the current (possibly unrestarted) configuration. |
| 8 | Multi-number data is a chart | Pass | The YAML is a configuration file, not a set of related measurements; rendering source text as text is the card's purpose. Its numbers are settings, not metrics. |
| 9 | Label: Value | **Fail** | The strings at `static.ts:2973` and `static.ts:2978` are full sentences, not "Label: Value" pairs or chart keys (principle 9 / checklist 9). Same root cause as #4, scored separately per the checklist. The title "Generated config (preview)" and the button labels are noun phrases and pass. |
| 10 | Copy buttons | **Fail** | The card renders the full generated YAML — the one artifact in this view a user copies wholesale (to diff, share, or hand-start the gateway) — with no copy affordance and no download: `static.ts:2974` has only the toggle, and the `<pre>` at `static.ts:2977` offers nothing but click-drag selection inside a 420px scroll region. Contrast the client endpoint card in the same view, which has Copy buttons for the base URL, LAN URL, and master key (`static.ts:2918`, `2927`, `2931`, wired to `navigator.clipboard.writeText` at `3140–3166`). |
| 11 | Status colors | Pass | No state chips or status colors; the load error uses `var(--err)` (`static.ts:2976`), which matches the error semantics. |
| 12 | Unknown is not zero | **Fail (minor)** | On open, the wiring sets `yamlText = null` and repaints before fetching (`static.ts:3080–3090`), so the expanded card transiently renders an empty `<pre>` (`static.ts:2977`, `esc(yamlText ?? "")`) — a present-but-empty element with no "loading" or "not measured" affordance. |
| 13 | Empty states | **Fail (minor)** | Before the preview is ever opened, the card still renders its full prose paragraph (`static.ts:2973`): placeholder explanation standing in for data the card has not loaded. The card chrome (title + toggle) is a legitimate expandable header; the prose under it is not. |
| 14 | Number formatting | Pass | The card chrome renders no numbers; the YAML body is verbatim file content, which formatting rules do not rewrite. |

**What already complies and must be preserved:** the preview is collapsed by default behind a toggle (principle 1); every Preview click refetches, so a cached copy can never hide config edits (`static.ts:3078–3089`, comment at 3078–3079); the YAML is HTML-escaped (`esc(...)`, `static.ts:2977`); the card sits last in the view (`static.ts:2981`).

## Redesign proposal

Keep the card, the toggle, and the refetch-on-open behavior. Replace the chrome around the `<pre>`:

### Layout

```
[icon] Generated config        Source: current configuration ⓘ    [Copy] [Download] [Hide YAML]
┌─ pre (only when expanded; max-height 420px, scrolls) ──────────────┐
│ model_list: …                                                       │
└─────────────────────────────────────────────────────────────────────┘
  api_key placeholder note → ⓘ beside the Copy button (see below)
```

- **Collapsed (default): one row.** Section title, the summary values that are known, and the action buttons. Nothing else — the card stops costing a prose paragraph of page length (fixes #1, #13).
- **Summary as Label: Value** (principle 9), shown in the header row once the YAML has been fetched at least once, omitted before that (principle 13 — omit, never fake):
  - `Lines: 128`
  - `Models: 5` (count of `model_name` entries in the fetched YAML — real data from the artifact, not a restatement of the summary card's group count)
  - `Source: current configuration` — always true of the preview; the restart caveat moves to this value's info-tip.
- **Expanded:** the existing `<pre>`, unchanged in styling, still refetched on every open.

### Narrative moves (fixes #4, #9)

- The two sentences at `static.ts:2973` move verbatim into an info-tip on the section title (or on `Source:`), matching the info-tip pattern already used in this view (e.g. `static.ts:2889`, `2926`). The drift warning is methodology — it explains itself on hover, not at rest.
- The `api_key: local` note at `static.ts:2978` moves verbatim into an info-tip attached to the expanded `<pre>`'s header row.
- The load error keeps `var(--err)` but becomes a value, not a sentence: `Preview: failed to load` in the header's state slot, with the raw error text as its `title` tooltip.

### Copy and download (fixes #10)

- **Copy button** in the header row, beside the toggle, using the established pattern (`navigator.clipboard.writeText`, as at `static.ts:3144`). If the YAML has not been fetched yet, Copy fetches it first (same call the toggle handler makes, `getGatewayYaml`), then writes it — the button never copies an empty string.
- **Download button** beside Copy: a `Blob` + temporary anchor download named `config.yaml`. This is the honest answer to principles 1 and 7 for this card: the config text earns *collapsed* space in the page (it is genuinely inspected in place — checking one deployment's `api_base` is the common case), but its primary uses are copy/download, so those actions belong in the header, not after a 420px scroll region. Removing the inline `<pre>` entirely was considered and rejected: in-place reading is a real use, and the toggle already keeps it out of the page until asked.

### Loading state (fixes #12)

- While a fetch is in flight after opening, render `Loading…` text inside the `<pre>` region (or omit the `<pre>` until the fetch resolves). An empty code box is neither an absent element nor an explicit affordance.

### Net effect

Collapsed card: one line. Expanded card: header + artifact + two actions the artifact exists for. No visible sentences anywhere; both explanations survive verbatim in tooltips; nothing about the data, escaping, or refetch behavior changes.

# Design Audit — Wizard Step Card

Card: one row per setup step in the Setup Wizard view.
Source: `wizardStepRow` in `ui/src/views/static.ts:3637-3676`, painted by `paintWizard` (same file), state shape `WizardStepState` in `ui/src/types.ts:128-149`, wire mapping `mapBackendStep` in `ui/src/api.ts:513-521`, styles in `ui/src/styles.css`.
Audited against: `docs/design-principles.md` Parts 1 and 3.
Date: 2026-10-16. Audit only — no code changed.

## Verdict

**Redesign needed.** 7 of 14 checklist questions fail (4, 9, 1, and 11 substantively; 2, 6, 10 as minors). The structural problem: the row is a text surface, not a state surface. The step's outcome message, remediation prose, a free-text note, a 3-line log tail, and a full manual-command block all render expanded inline, and the one transitional state (running) is colored blue instead of the amber the status semantics require.

| # | Question | Result |
|---|---|---|
| 1 | Compact | FAIL |
| 2 | One section, multiple views | FAIL (minor) |
| 3 | Prime real estate | pass |
| 4 | Narrative in tooltips | FAIL |
| 5 | Real data | pass |
| 6 | Numbers in one place | FAIL (minor) |
| 7 | Density with order | pass |
| 8 | Multi-number data is a chart | pass |
| 9 | Label: Value | FAIL |
| 10 | Copy buttons | FAIL (minor) |
| 11 | Status colors | FAIL |
| 12 | Unknown is not zero | pass |
| 13 | Empty states | pass |
| 14 | Number formatting | pass |

## Findings by question

### 1. Compact — FAIL

A failed step expands in place with three stacked blocks: the remediation div (`static.ts:3651-3652`), the last-3-lines log tail (`static.ts:3653-3654`), and the manual-command block with a full `<pre>` of PowerShell (`static.ts:3660-3663`). Every block pushes all later steps down the page. All three are detail-on-demand content and belong behind one collapsed expandable per step; none is.

### 2. One section, multiple views — FAIL (minor)

The step's log output is presented twice in the same view: the truncated tail inside the row (`static.ts:3653-3654`) and the live log stream (`#wizLog`, `static.ts:3794`) at the bottom of the same wizard panel, which receives the same steps' output during runs. One data set, two stacked presentations.

### 3. Prime real estate — pass

The row leads with the state icon and step name, with the state value right-aligned (`static.ts:3664-3666`; grid in `styles.css:360-364`). The first-read elements are exactly the card's question: which step, what state.

### 4. Narrative in tooltips — FAIL

- `detail` is the step's outcome `message` — a full sentence (e.g. a failure reason) — mapped at `api.ts:517` (`detail: s.message`) and always rendered inline under the name (`static.ts:3665`, `.sdetail`, `styles.css:366`).
- `remediation` is instructional prose rendered inline on failure (`static.ts:3651-3652`). Source strings are sentences, e.g. `crates/manager-wizard/src/steps.rs:606` ("install a current NVIDIA driver (R550+, i.e. 551.61+) and re-run the hardware check — download: https://…") and `steps.rs:834` ("re-run install-llamacpp to re-download and re-verify the build; if llama-server fails to start, install the VC++ redistributable first").
- The done-state `note` is free text (from the wizard's notes map, `crates/manager-wizard/src/lib.rs:908`) rendered as a chip containing the whole sentence (`static.ts:3665`).

None of this sits behind a tooltip, info-tip, or collapsed expandable.

### 5. Real data — pass

Progress segments map 1:1 to steps and their states (`paintWizard`, `static.ts:3749-3750`), the percentage is computed from the done count, and durations come from measured `duration_ms`. Observation, not a failure under this question's terms: the wrench glyph repeated on every row (`static.ts:3664`, second 28px grid column) is constant decoration that encodes nothing; the redesign removes it.

### 6. Numbers in one place — FAIL (minor)

- The NVIDIA remediation sentence embeds the driver-download URL as raw text (`steps.rs:606`) while the identical URL is the step's `helpUrl` behind the "Open driver download" button (`static.ts:3646-3647`) — the same datum twice in one card.
- Progress is stated three ways in the header: done count, total, and percentage text, plus the segmented bar (`static.ts:3749-3750`).
- The log duplication in finding 2 is also a numbers-in-one-place violation for the tail lines.

### 7. Density with order — pass

Content is grouped per step; the summary (name, state) leads and result detail follows. The on-demand failure is scored under questions 1 and 4, not double-counted here.

### 8. Multi-number data is a chart — pass

The only multi-number set in the wizard (progress across steps) is rendered as the segmented bar in the container. The row itself carries a single number (duration). Log-tail lines are log text, not a numeric set.

### 9. Label: Value — FAIL

Visible strings that read as sentences rather than "Label: Value" or a chart key:

- The outcome message line (`.sdetail`, `static.ts:3665`) — a sentence in every state that has a message.
- The remediation line (`static.ts:3651-3652`) — a sentence.
- The done-state note chip (`static.ts:3665`) — a sentence inside a chip.
- The manual block's header, "Run this step manually (PowerShell):" (`static.ts:3660`) — an imperative sentence used as a label; the noun form is "Manual commands (PowerShell):".

The state text itself is compliant: "done · 45s", "manual", "failed", "queued" (`static.ts:3642-3645`).

### 10. Copy buttons — FAIL (minor)

The headline case is satisfied: the manual-command block carries a "Copy commands" button (`data-copy-step`, `static.ts:3663`), and commands inside the block are copied as one unit — correct treatment for a command block. The failure is the remediation URL: the driver-download URL appears as raw sentence text (`steps.rs:606`) with no copy button beside it; its only affordance is the separate Open button via `helpUrl`. A URL rendered as a value needs the copy affordance (principle 10), or should not be rendered as text at all when the button already carries it.

### 11. Status colors — FAIL

- **Running is blue, not amber.** Principle 11 assigns transitional states to amber. The running icon is `var(--accent)` (`static.ts:3639`), and `.step.running` uses the accent for border, background tint, and state text (`styles.css:369-370`); the progress bar's running segment is also accent (`styles.css:356`). `--accent` is `#4a9eff` (`styles.css:13`); the amber token `--warn: #d29922` exists (`styles.css:15`) and is what starting/stopping states use elsewhere.
- **Failed uses the saturated error red** for border and state text (`.step.failed`, `styles.css:372-373`, `--err: #f85149`, `styles.css:16`) rather than the light-red treatment the chip semantics specify for stopped/failed.
- **The done-state note chip is amber** (`badge-warn`, `static.ts:3665`) — a completed step carrying the transitional color, which muddies what amber means in the same view where a running step should hold it.
- Compliant: done is green (`.step.done .sstate`, `styles.css:368`; check icon `--ok`, `static.ts:3638`), queued is dimmed gray (`styles.css:371`), skipped falls through to the gray neutral icon/text, and "manual" takes the green check as a completed-equivalent state. No chip states a default: the note chip exists only when a note exists.

### 12. Unknown is not zero — pass

A zero `duration_ms` renders no duration at all (`api.ts:519`). Steps blanked during a run (the running step, and stale steps during a full run/audit) render an honest current state with empty detail instead of a stale or zero value (`paintWizard`'s `blank()`, `static.ts:3730-3734`). No unmeasured value renders as 0 anywhere in the row.

### 13. Empty states — pass

Remediation, log tail, manual block, actions, and the note chip are all conditional and omitted when absent (`static.ts:3646-3663`). Nit: `.sdetail` renders as an empty div for blanked steps (`static.ts:3665`) — an empty element, not placeholder prose, so the principle's letter holds.

### 14. Number formatting — pass

Durations pass through `trim1` (`api.ts:519`; `ui/src/charts.ts:132-137`: whole at magnitude ≥100, at most one decimal, trailing `.0` dropped) and render as e.g. "45s" / "1.5s". Progress counts and the percentage are integers (`static.ts:3749`). Log-tail content is raw log text, not formatted values.

## Proposed redesign

Same data, same actions (Run/Retry, Open driver download, Copy commands), one expandable per step.

**Row (always visible, fixed content):**

```
[status icon]  Step name                          done · 45s   [▾]
```

- Grid drops the constant wrench column (encodes nothing): `[icon] [name] [state] [disclosure chevron]`.
- State value stays right-aligned mono: `done · 45s`, `failed`, `running…`, `queued`, `skipped`, `manual`.
- Colors per principle 11: running icon/border/segbar segment → `--warn` amber; failed → light-red treatment matching the backend chips (pale red tint, `--err` text); done → green (unchanged); queued/skipped → gray (unchanged); manual → green check (unchanged).
- The outcome message, note, remediation, log tail, and manual block leave the row surface entirely.

**Expandable ("Details", collapsed by default; auto-open only for the step a paused run is blocked on):**

- `Outcome:` — the step message, as prose inside the expandable (principle 5 permits narrative there).
- `Note:` — the current note-chip sentence, shown only when a note exists. A done step with a note additionally shows a small amber info icon in the row whose tooltip is the note — the row signals "there is a note" without printing it.
- `Fix:` — the remediation prose. When the step has a `helpUrl`, the URL is stripped from the sentence text (it duplicates the button) and the existing "Open driver download" button sits beside the line; any URL that must appear as a value gets a copy button beside it.
- `Log (last 3 lines):` — the mono tail block, plus an "Open in Logs view" affordance for the full stream. The tail no longer also lives outside the expandable.
- `Manual commands (PowerShell):` — noun label, the existing `<pre>`, and the existing Copy commands button moved up beside the label (label left, copy right). Outcome-supplied manual blocks keep their own first line as today.
- Run/Retry and help actions stay in the row's action area (they are the step's primary actions, not detail).

**Adjacent container fix (same paint pass):** the progress header drops the "· NN%" text — the done count and the segmented bar already encode progress, in one place each (`static.ts:3749-3750`).

**What does not change:** the `#wizLog` live log stream at the panel bottom (it is the run console, not card content), the blanking behavior during runs, the busy-state disabling, sample-mode data, and every command the row can invoke.

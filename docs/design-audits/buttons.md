# Design Audit — Buttons (rule 15 sweep)

Scope: every `<button>` in `ui/src` (main.ts shell + activation banner, views/dashboard.ts, views/static.ts; views/serviceMap.ts renders no buttons)
Reference: `docs/design-principles.md` — rule 15 (Icons over words for safe actions) and checklist question 15, added 2026-10-17
Date: 2026-10-17
Verdict: **Brought into compliance** — 68 render sites converted to icon-only, 16 render sites gained an icon beside retained words, 12 icons added to the set. No ids, `data-*` hooks, handlers, or click semantics changed; the single handler-body touch is noted under Judgment calls (6). **Superseded in part by the 2026-10-17 addendum at the end of this document** (Pete's corrections to rule 15): five sites restored to icon + word and the Decide later button replaced by a dismiss X — final counts are 63 icon-only conversions and 21 words + icon sites.

## Rule applied

- **Non-destructive, not operation-critical** → icon-only, with `title` (tooltip) and `aria-label` naming the action.
- **Destructive or operation-critical** (lifecycle start/stop/restart, apply, drain, cancel changes, form commits, step execution) → words stay, an icon is added if missing.
- Already compliant buttons were not touched.

## Effect count

| Direction | Render sites |
|---|---|
| Words (or words+icon) → icon-only | 68 (63 after the 2026-10-17 addendum) |
| Words only → words + icon (critical/destructive) | 16 (21 after the addendum's restorations) |
| Already compliant, untouched | per inventory tables |
| Kept words by scope decision (tabs, mode selectors, menu items) | per inventory tables |

New icons in `ui/src/icons.ts`, same stroke style: `copy`, `eye`, `eyeOff`, `x`, `plus`, `chevL`, `chevR`, `chevD`, `pencil`, `folder`, `external`, `peak`.

## Inventory and disposition

### App shell — activation banner (`main.ts`)

| Button | Class | Before | After |
|---|---|---|---|
| Drain Requests and Apply (`data-ab-act="drain"`) | Critical | words | words + `pause` icon |
| Apply now (`data-ab-act="apply-now"`) | Critical | words | words + `restart` icon |
| Apply plan (`data-ab-act="apply"`) | Critical | words | words + `play` icon |
| Review (`data-ab-act="review"`) | Safe (navigation) | words | icon-only `eye` → **words + `eye`** (restored — addendum C1) |
| Copy commands (`data-ab-act="copy-manual"`) | Safe (copy) | words | icon-only `copy` |

The banner's Apply forms now match the plan card's `planApplyButtons`, which already carried the same icons.

### Dashboard (`views/dashboard.ts`)

| Button | Class | Before | After |
|---|---|---|---|
| Restart (service card, `data-act="restart"`) | Critical | words + icon | unchanged |
| Logs (service card, `data-act="logs"`) | Safe (navigation) | words + icon | icon-only `logs` |
| Trends toggle (GPU strip card, `data-trends`) | Safe (expand) | glyph + words | icon-only `chevD`/`chevR` |
| Pause live / Resume live (`#pauseBtn`) | Safe (transport toggle) | words + icon | icon-only `pause`/`play`, name in tooltip |
| View tabs (Appliance / Console / Topology) | Tab idiom | words | unchanged — scope decision (below) |

### GPUs view (`renderGpus`)

| Button | Class | Before | After |
|---|---|---|---|
| Refresh (`#gpuRefresh`) | Safe | words + icon | icon-only `restart` |
| Reset peaks (`#gpuResetPeaks`) | Safe | words | icon-only `peak` (new glyph, call 9) |
| Snapshot (`#gpuSnapshot`) | Safe | words + icon | icon-only `download` |

### Activation plan card + Activation Plan view (`activationPlanHtml`, `wireActivationPlan`)

| Button | Class | Before | After |
|---|---|---|---|
| Drain Requests and Apply / Apply now / Apply plan (`planApplyButtons`) | Critical | words + icon | unchanged |
| Cancel changes / Confirm cancel changes | Destructive | words | words + `ban` icon |
| Decide later (`data-plan-act="later"`) | Safe (dismiss) | words | icon-only `clock` → **removed; replaced by an icon-only `x` Dismiss at the card's top-right, same hook** (addendum C2) |
| Review (summary, dismissed state) | Safe (expand) | words | icon-only `eye` → **words + `eye`** (restored — addendum C1) |
| Refresh (`data-plan-refresh`, 3 render sites) | Safe | words | icon-only `restart` |
| Copy commands (`manualCommandHtml`) | Safe (copy) | words | icon-only `copy` |

The summary variant's non-dismissed Review is an `<a href="#/activation-plan">`, a link — out of scope (below).

### Backend card + Backends view (`backendCard`, `renderBackends`, add-backend form)

| Button | Class | Before | After |
|---|---|---|---|
| Stop / Start / Restart | Critical | words + icon | unchanged |
| Test (`data-back-act="test"`) | Safe (probe) | words + icon | icon-only `bolt` (call 3) |
| Enable (`data-back-act="enable"`) | Safe (toggle) | words + icon | icon-only `check` (call 4) → **words + `check`** (restored — addendum C3) |
| Disable (`data-back-act="disable"`) | Safe (toggle) | icon-only | unchanged |
| Remove (`data-back-act="remove"`) | Destructive | icon-only | unchanged |
| Advanced options menu toggle | Safe | icon-only | unchanged |
| Menu items: Edit flags / Placement / Launch command | Menu idiom | words + icon | unchanged — scope decision (below) |
| Details toggle (`data-back-act="details"`) | Safe (expand) | glyph + words | icon-only `chevD`/`chevR` (call 5) |
| Save flags (`flags-save`) | Critical (form commit) | words | words + `check` icon |
| Cancel (flags form) | Safe | words | icon-only `x` |
| Propose split (`place-propose`) | Safe (computation) | words | icon-only `search` (call 13) → **words + `search`** (restored — addendum C4) |
| Apply split (`place-apply`) | Critical (apply) | words | words + `check` icon |
| Close (placement form) | Safe | words | icon-only `x` |
| Add backend (`#backAdd`, topbar) | Safe (opens form) | words + icon | icon-only `plus` → **words + `plus`** (restored — addendum C5) |
| Refresh (`#backRefresh`) | Safe | words + icon | icon-only `restart` |
| Add backend (`#addBackSubmit`) | Critical (form commit) | words | words + `plus` icon |
| Cancel (`#addBackCancel`, 2 forms) | Safe | words | icon-only `x` |
| Done (`#addBackDone`) | Safe (dismiss) | words | icon-only `check` |

### Models view (`modelCard`, `renderModels`)

| Button | Class | Before | After |
|---|---|---|---|
| Download (not on disk) | Critical-family (commit) | words + icon | unchanged (call 1) |
| Pause download | Safe (transport) | words + icon | icon-only `pause` (call 2) |
| Resume download (paused + partial states) | Safe (transport) | words + icon | icon-only `play` (call 2) |
| Cancel download (2 render sites) | Destructive (discards partial) | words | words + `x` icon |
| Delete | Destructive | words + icon | unchanged |
| Verify checksum | Safe (read-only check) | words + icon | icon-only `shield` (call 14) |
| Enable / Disable (`toggle-enabled`) | Safe (toggle) | words | icon-only `check` / `ban` (call 4) |
| Rename (`rename`) | Safe (edit-open) | words + icon | icon-only `pencil` (call 10) |
| Save name (`rename-save`) | Critical (form commit) | words | words + `check` icon |
| Cancel (`rename-cancel`) | Safe | words | icon-only `x` |
| Copy hash (card header + Details) | Safe (copy) | words | icon-only `copy` |
| Copy (file path) | Safe (copy) | words | icon-only `copy` |
| Details toggle | Safe (expand) | glyph + words | icon-only `chevD`/`chevR` (call 5) |
| Optional fields toggle (`#addModelOptToggle`) | Safe (expand) | glyph + words | icon-only `chevD`/`chevR` (calls 5, 6) |
| Add model (`#addModelBtn`) | Critical (form commit) | words | words + `plus` icon |

### Logs view (`renderLogs`, `wireLogs` incl. the calendar picker)

| Button | Class | Before | After |
|---|---|---|---|
| Calendar pickers (`#logFromCal`, `#logToCal`) | Safe | icon-only | unchanged form; `aria-label` added |
| Prev / Next (`#logPrev`, `#logNext`) | Safe (pagination) | words | icon-only `chevL` / `chevR` |
| Refresh (`#logRefresh`) | Safe | words + icon | icon-only `restart` |
| Export diagnostics bundle (`#logExport`) | Safe | words + icon | icon-only `download` (call 16) |
| Open folder (`#logOpenFolder`) | Safe (navigation) | words | icon-only `folder` |
| Picker month nav (`data-nav`, 2) | Safe | text glyph | `chevL` / `chevR` icons |
| Picker Clear (`data-act="clear"`) | Safe | words | icon-only `x` |
| Picker Apply (`data-act="apply"`) | Critical-family (apply) | words | words + `check` icon |
| Picker day cells (`dt-day`) | Data, not actions | day number | unchanged — the number is the datum |

### Gateway view (`gatewayPaint`)

| Button | Class | Before | After |
|---|---|---|---|
| Start / Restart gateway (`#gwStartBtn`) | Critical | words + icon | unchanged |
| Stop gateway (`#gwStopBtn`) | Critical | words + icon | unchanged |
| LiteLLM admin UI (`#gwAdminUiBtn`) | Safe (opens page) | words + icon | icon-only `gateway` (call 7) |
| Copy (`#gwBaseUrlCopyBtn`, `#gwLanUrlCopyBtn`, `#gwKeyCopyBtn`, `#gwOrCredCopyBtn`) | Safe (copy) | words | icon-only `copy` |
| Reveal / Hide master key (`#gwKeyRevealBtn`) | Safe (reveal) | words | icon-only `eye` / `eyeOff` |
| Enable / Disable tier (`#gwOrToggle`) | Safe (toggle) | words | icon-only `check` / `ban` (call 4) |
| Model allowlist toggle (`data-or-models`) | Safe (expand) | glyph | icon-only `chevD`/`chevR` (call 5) |
| Fragment toggle (`data-fragment`) | Safe (expand) | glyph + words | icon-only `chevD`/`chevR` (call 5) |
| Copy (`#gwYamlCopyBtn`) | Safe (copy) | words | icon-only `copy` |
| Download (`#gwYamlDownloadBtn`) | Safe | words | icon-only `download` |
| Preview LiteLLM YAML / Hide YAML (`#gwYamlBtn`) | Safe (view toggle) | words | icon-only `eye` / `eyeOff` |
| Routing tabs (Flow / Groups) | Tab idiom | words | unchanged — scope decision (below) |

### Settings view (`settingsPaint`, `mxcPolicyInner`)

| Button | Class | Before | After |
|---|---|---|---|
| Copy (`data-copy-value`) | Safe (copy) | words | icon-only `copy` |
| Show all N rules / Hide rules (`data-rules-toggle`) | Safe (expand) | glyph + words | icon-only `chevR`/`chevD`; the full text incl. count rides the tooltip (call 5) |
| All configuration values (`data-all-toggle`) | Safe (expand) | glyph + words | icon-only `chevD`/`chevR` (call 5) |
| Export diagnostics bundle (`#setExportBtn`) | Safe | words + icon | icon-only `download` (call 16) |
| Open folder (`#setOpenFolder`) | Safe (navigation) | words | icon-only `folder` |
| Run self-test (`data-mxc-selftest`) | Safe (probe) | words | icon-only `shield` (call 3) |
| MXC mode buttons (Learning / Enforced / Disabled) | Selector idiom | words | unchanged — scope decision (below) |
| Tabs (settings tabs, rules filter) | Tab idiom | words | unchanged — scope decision (below) |

### Setup wizard (`wizardStepRow`, `renderWizard`)

| Button | Class | Before | After |
|---|---|---|---|
| Run / Retry step (`data-run-step`) | Critical (executes a step) | words | words + `play` / `restart` icon |
| Open driver download (`data-help-url`) | Safe (opens page) | words | icon-only `external` |
| Copy commands (`data-copy-step`) | Safe (copy) | words | icon-only `copy` |
| Details chevron (`data-wiz-details`) | Safe (expand) | text glyph | `chevD`/`chevR` icons; `aria-label` added |
| Re-run audit (`#auditBtn`) | Safe (read-only check) | words + icon | icon-only `search` (call 12) |
| Run setup (`#runBtn`) | Critical | words | words + `play` icon |
| Finish (`#finishBtn`) | Critical | words | words + `check` icon |
| Back (footer, permanently disabled) | Safe (navigation) | words | icon-only `chevL` (call 11) |

### Cross-view shared elements

| Element | Disposition |
|---|---|
| Overview status buttons (`ovStatusBtn`, GPUs + Backends + Models overviews) | Already icon-only (dot + icon, tooltip) — unchanged |
| Dashboard Start All bar | Not present in this tree (it lives on the `feature/windows-services` branch); when it lands it is a lifecycle action → words + icon |

## Judgment calls (flagged for review, decided as noted)

1. **Model Download kept words + icon.** A download commits the machine to a multi-GB transfer; classified with the Apply/commit family rather than the copy/reveal safe family. Easiest call to reverse if Pete disagrees.
2. **Download Pause/Resume and dashboard Pause live → icon-only.** Reversible transport controls in the universal play/pause idiom; state is carried by the card's state chip and the icon swap itself.
3. **Probes → icon-only.** Backend Test (`bolt`) and MXC Run self-test (`shield`) are non-destructive checks whose results land as card/view notes.
4. **Enable/Disable symmetry.** Backend Disable, model toggle, tier toggle were inconsistent (Disable icon-only on backend cards, words elsewhere). All enable/disable toggles are now icon-only (`check` to enable, `ban` to disable), the direction named in the tooltip and aria-label.
5. **Expand/collapse family → chevron icon-only, uniformly.** Details (backend, model, wizard step), Trends, Fragment, allowlist, Optional fields, All configuration values, Show-all-rules: the chevron direction carries open/closed, the content name rides `title` + `aria-label`. For Show-all-rules, the rule count moved into the tooltip ("Show all N rules").
6. **One handler-body touch.** The add-model Optional-fields toggle previously rewrote its own text label on click; it now swaps the chevron via `innerHTML`. Same control, same click semantics — the label update was part of the button's rendering, which is what changed.
7. **LiteLLM admin UI → icon-only** despite sitting in the gateway lifecycle row: it only opens a page in the browser. Start/Stop beside it keep words + icons.
8. **Refresh keeps the `restart` circular-arrow glyph** — pre-existing house usage (Refresh buttons already used it). Restart buttons all keep their words, so the shared glyph is always disambiguated by label or tooltip.
9. **Reset peaks got a new `peak` glyph** — no existing icon fit, and reusing the refresh arrow would have put two identical glyphs side by side in the GPUs topbar.
10. **Rename uses a new `pencil` glyph**, not the existing `wrench`: the wrench already denotes the Setup Wizard / settings in this UI, and Rename is an edit-open action.
11. **Wizard Back (disabled placeholder) converted** to an icon-only chevron for consistency; it remains disabled and inert.
12. **Re-run audit → icon-only.** A read-only re-check; the wizard's critical actions (Run setup, Finish, per-step Run/Retry) keep words + icons.
13. **Propose split → icon-only (`search`).** It computes a projection and changes nothing; Apply split, the commit, keeps words + icon.
14. **Verify checksum → icon-only (`shield`).** A read-only check; its result lands as the card note.

## Scope decisions (kept words, deliberately)

- **Tabs** (dashboard view switcher, Routing Flow/Groups, Settings tabs, MXC rules filter): tab labels name views/states, not actions; the tab idiom is text. Not action buttons under rule 15.
- **Segmented selectors** (MXC mode: Learning/Enforced/Disabled): the label is the value being selected; icon-only would hide the state space.
- **Menu items** (backend card's advanced menu): menus follow the icon + label idiom; the menu *toggle* is icon-only.
- **Anchors styled as buttons** (sidebar nav, Setup Wizard links, "Open Gateway view", plan summary Review link, "Add a model", dashboard "Setup wizard"): these are links (`<a href>`), not buttons — untouched.
- **Calendar day cells**: the number is data, not an action label.

## Behavior preservation

Every changed button keeps its `id`, `data-*` hook, disabled-state logic, and handler; only content (icon vs words), `title`, `aria-label`, and — where a standard action-row button became icon-only — the existing `.btn.icon-only` class changed. Small in-row buttons kept their compact inline padding instead of the 32px `.icon-only` box. Surfaces called out in the task verified by construction and gate: Logs calendar popup (nav/clear/apply + day cells), copy buttons, pagination, plan actions (card + banner share the same hooks), dashboard Pause live and service-card actions. (The Start All bar does not exist in this tree — see inventory.)

Gates: `npm run build` (tsc + vite) pass; `npm run smoke` pass.

## Addendum — Pete's corrections (2026-10-17)

Pete reviewed this sweep and refined the rule. Rule 15 in `docs/design-principles.md` (and checklist question 15) now carries the carve-out: **the standard workflow action of a surface — the action the user most likely wants to take next on it — keeps its words AND its icon even when non-destructive; icon-only is for secondary safe actions.** The corrections below apply that refinement. Handler hooks, ids, and click semantics are unchanged throughout; restorations change only the button content and drop the now-redundant `aria-label` (the visible word names the action), keeping `title`.

- **C1 — Review restored to icon + word (2 sites).** Pete named this one: for activation plans, Review is the most common action in the header banner. The banner's Review (`main.ts`, `data-ab-act="review"`) and the plan card's dismissed-state Review (`data-plan-act="review"`) are the same action on surfaces the code deliberately keeps in parity (the banner renders the card's summary variant), so both regained the word beside the `eye` icon. This supersedes their "Safe (navigation/expand)" classification above: Review is the activation surfaces' standard workflow action.
- **C2 — Decide later removed; dismiss X at the plan card's top-right.** Verified in code first: Decide later (`handlePlanAction`, `ui/src/views/static.ts`) did nothing but dismiss — it set the view-local `PlanUiState.dismissed` flag (plus cleared a pending cancel-confirm and the status line), repainted the full plan card as its one-line summary variant, and made no backend call and persisted nothing. Scope: in-memory, per view instance — it survived repaints and the Activation Plan view's 10 s poll within one mount, reset on navigation, and never hid the global banner. Per Pete's direction the labeled button is gone from the card's action row; an icon-only `x` button now sits at the top-right of the card's header with the **same `data-plan-act="later"` hook**, so the dismissal semantics are identical by construction. The `clock` icon remains in the set (used by loading states).
- **C3 — Backend Enable restored to icon + word.** On a disabled backend's card, Enable is the only workflow action offered: Start renders only for enabled backends, and the card's remaining controls are the Remove icon and the advanced menu. It is therefore that card state's standard workflow action. This deliberately breaks the enable/disable symmetry of call 4 for this one surface — Disable stays icon-only because it renders beside the worded Start on a stopped/failed enabled backend's card, where Start is the primary action and Disable is the secondary state toggle. The model and OpenRouter tier enable/disable toggles also stay icon-only: their cards' primary actions (Download on the model card) already carry words.
- **C4 — Propose split restored to icon + word.** In the placement form, Apply split is disabled until a projection exists, so Propose split is the gating first step — the action the user most likely takes next when the form opens. Supersedes call 13.
- **C5 — Add backend (Backends topbar) restored to icon + word.** It is the Backends view topbar's workflow action and opens the add workflow whose commit button (`#addBackSubmit`) is worded; the Models view's equivalent opener ("Add a model") also carries its word. `#backRefresh` beside it stays icon-only.

### Borderline cases left icon-only (reviewed against the refined rule, deliberately unchanged)

- **Done (`#addBackDone`)** — the sole button on the post-add confirmation card, but a post-completion dismissal: the add workflow's commit (`#addBackSubmit`) carries the words, and Done belongs to the dismiss family (form Cancel / Close) that stayed icon-only. Sole-action status alone did not make it a workflow action.
- **Refresh on the plan fallback cards** (`wireActivationPlan` "unavailable" and "no plan pending" states) — Refresh is a uniform icon-only utility across every view; on the error card the primary content is the error itself, and singling out this Refresh would break the idiom. The in-card `data-plan-refresh` beside Apply/Cancel is plainly secondary and also unchanged.
- **Re-run audit (`#auditBtn`, wizard)** — a read-only re-check of the checklist; the wizard's workflow actions (Run setup, per-step Run/Retry, Finish) all carry words + icons.
- **Export diagnostics bundle (Logs `#logExport`, Settings `#setExportBtn`)** — a secondary utility on both surfaces; the Logs workflow is read/filter, and Settings has no single primary action.
- **GPUs topbar (Refresh / Reset peaks / Snapshot)** — a read-only telemetry surface; all three are utilities, none is a workflow step.
- **Dashboard service-card Logs** — secondary navigation beside the worded lifecycle actions (Start/Stop/Restart).
- **LiteLLM admin UI (Gateway)** — opens an external page; the gateway lifecycle actions beside it carry words (call 7 stands).
- **Pause/Resume transport toggles** (dashboard live, model downloads) — call 2 stands; play/pause idiom, state carried by the icon swap and the card chip.

### Observed, not changed

- The plan summary's non-dismissed Review is an `<a href="#/activation-plan">` carrying the word but no icon. Anchors were scoped out of this sweep (see Scope decisions); with C1 the three Review renderings are worded everywhere, and icon parity for that one link is the only remaining difference.

### Updated counts

| Direction | Render sites (final) |
|---|---|
| Words (or words+icon) → icon-only | 63 (68 swept, 5 restored, Decide later's `clock` removed and the dismiss `x` added in its place) |
| Words + icon (critical/destructive + restored standard-workflow) | 21 (16 swept + 5 restored) |

Gates after the corrections: `npm run build` (tsc + vite) pass; `npm run smoke` pass.

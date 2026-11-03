# Design rule 17 audit — form close on save (2026-10-21)

Rule 17 (design-principles.md, Controls): saving or applying any form closes
it and displays the object or block in its new state; on a failed save the
form stays open with the operator's input intact and the error shown. Scope:
object-editing forms with an explicit save/apply action. Out of scope: the
rule-16 enable switch, immediate-apply controls that commit on change, and
multi-step flows (the setup wizard advances rather than closing). A field
that lives permanently on a persistent card/panel with its own Save/Apply
has nothing to close; on success the saved value redisplays in its new
state, and on failure the operator's input stays intact with the error
shown, never discarded by a repaint.

Completion record: the rule and its fixes merged to main in v0.1.45 (merge
1b032ca; the branch tree was verified tree-identical to the merged content).
Rule numbering resolved at that merge: rule 17 is this rule (it merged
first); the Groups pages-flow rule was renumbered to 18. This audit was
completed against main at v0.1.46 (4fbdfac): every surface below was checked
against the rule, including the KV cache / flash-attention controls that
landed in v0.1.46 after the original audit pass.

Classification key: FORM = object-editing form with explicit save/apply;
PERM = permanent field on a persistent card/panel with its own Save/Apply
(nothing to close; redisplay on success, preserve on failure); IMM =
immediate-apply control committing on change (rule 17 out of scope);
SWITCH = rule-16 enable switch (out of scope); ACTION = action/job, not a
form.

| Surface (save path) | Class | Verdict | Evidence |
|---|---|---|---|
| Models — Add model (static.ts wireModels) | FORM | Compliant (baseline, Oct 20 fix) | Success clears+hides; failure keeps input+error |
| Models — Rename short name (rename-save) | FORM | Compliant (baseline) | Success closes (renameFor=null); failure keeps form, draft in wire state, error note |
| Models — MoE offload declaration (offload-save) | FORM | Compliant (baseline) | Success closes; failure and validation failure keep form, wire drafts intact, error note |
| Backends — Add backend (addBackSubmit → backend_add) | FORM | Compliant (baseline) | Success replaces form with confirmation card + list repaint; failure shows error in form, form stays (host outside repainted DOM) |
| Backends — consolidated Edit, structured Save (flags-save) | FORM | Compliant (baseline, Oct 20 declutter) | Success closeEditSession; failure keeps open, drafts in wire state, error on card |
| Backends — Edit, raw flags Save (flags-raw-save) | FORM | Compliant (baseline) | Success closes Edit session; failure keeps open, raw draft in wire state |
| Backends — Placement Apply split (place-apply) | FORM (part of Edit session) | Compliant (baseline) | Success closes placement form + repaint; failure keeps form+projection, error on card |
| Backends — Edit in-form offload slider / MTP select / parallel field / ctx-batch | IMM | Out of scope | Commit on release/change; rejected commit reverts draft to saved value, error on card, Edit stays open (they are not the form's Save) |
| Backends — Edit KV cache / flash-attention selects (data-cache-field → backend_set_cache) | IMM | Compliant (verified in completion pass vs v0.1.46) | Commit-on-change in the MTP/parallel idiom (saveCacheSelection/commitCacheSelection): success sets the draft to the committed selection, card message shows the saved tokens, list reloads; a rejected commit reverts the draft to the saved selection and paints `KV cache not saved: …` on the card, Edit session stays open; quantized-V + FA-off is named in the form and reverted without committing. Rule 17 failure semantics pinned in smoke.mjs |
| Backends — Edit predicted-best button (data-cache-best) | IMM (action) | Compliant (completion pass) | Applies the preview's best rung only on click, through the same commit path (same success/failure semantics as the selects); nothing is applied without the click |
| Backends — Add form KV cache / flash-attention selects + predicted-best (data-add-cache-field / data-add-cache-best → pCache draft carried by backend_add) | FORM (draft fields of the Add backend form) | Compliant (completion pass) | Selections sync into the wire draft (pCache), so repaints re-render the draft; the predicted-best button sets the draft only; quantized-V + FA-off is refused at submit with the error in the form and the form stays open; the Add form's overall success/failure behavior is the audited addBackSubmit row above |
| Groups — Create group (groups.ts doCreate) | FORM | Fixed in this build (e347eac / merged 5bc9835) | Before: form closed on success, but the typed name lived only in DOM — a failed create (or 10s poll repaint) cleared it. After: draft in wire state, input renders from draft, success closes+clears draft, failure re-renders draft+error |
| Groups — Move to menu / drag-and-drop, Delete group | IMM/ACTION | Out of scope | Immediate ops with outcome status; no form |
| Gateway — Routing rule Add/Edit (gwRuleSaveBtn → patchRules) | FORM | Fixed in this build (64fe2ed / merged 4e1ce5b) | Before: closed BEFORE the save outcome was known (ruleForm.open=false ahead of patchRules), so a failed/non-persisted save closed the form. After: patchRules returns a persisted bool; the form closes only on a persisted save; draft re-renders intact on failure, error in #gwMsg (outside repainted body) |
| Gateway — permanent fields via gwSave: Port Apply, OpenRouter credential Save, daily cap Save, allowlist Save, per-group Fallbacks Save | PERM | Fixed in this build (64fe2ed / merged 4e1ce5b) | Before: gwSave repainted on failure too, discarding the typed value and redisplaying the old saved value. After: no repaint on error/non-persisted (input intact, error in #gwMsg); repaint only on success |
| Gateway — Network access bind select (gwBindSelect) | IMM | Out of scope | Commits on change; error shown in #gwMsg |
| Gateway — OpenRouter enable, rule enable toggles | SWITCH | Out of scope | Rule 16 switch |
| Gateway — routing rule Delete, Start/Stop gateway | ACTION | Out of scope | Confirm-arm / job actions |
| Settings General — Telemetry Apply, Windows ML Apply, PostgreSQL Apply (setTel/Winml/PgApply) | PERM | Compliant (baseline) | No repaint on failure (return + re-enable); repaint on success |
| Settings Updates — Component pin Save (data-comp-pin-save) | PERM | Compliant (baseline) | No repaint on failure; repaint on success |
| Settings Updates — LLMdil update toggles, component auto-update toggles, Unpin | SWITCH/IMM | Out of scope | Toggles; repaint on failure redisplays actual saved state (correct switch semantics) |
| Settings MXC — Policy file path Apply (setMxcPathApply) | PERM | Compliant (baseline) | No repaint on failure; repaint on success |
| Settings MXC — mode segmented buttons, self-test | IMM/ACTION | Out of scope | Mode commits on click with confirm for disable; failure no repaint |
| Settings Notifications — toggles, Send test | SWITCH/ACTION | Out of scope | Immediate toggles |
| Settings Supervision — mode buttons; Register (svcPwd password) / Unregister | IMM/ACTION with credential field | Compliant (baseline); not a form | Register failure returns without repaint (password field untouched in DOM); success repaints |
| Settings Remote Access — Admin API port Apply (data-ra-port-save) | PERM | Compliant (baseline) | No repaint on failure; repaint on success |
| Settings Remote Access — Create user (data-ra-create) | FORM (permanent create form) | Fixed in this build (64fe2ed / merged 4e1ce5b) | Before: handler repainted (load) on failure too, discarding username/password/role. After: no repaint on failure (input intact, error in #setMsg outside repainted body); success repaints, new user displays, form clears |
| Settings Remote Access — per-user role Apply (data-ra-apply-role) | PERM | Fixed in this build (64fe2ed / merged 4e1ce5b) | Before: repainted on failure, chosen role discarded. After: no repaint on failure; success repaints the row in its new state |
| Settings Remote Access — user enable switch, Reset password, Pairing code, Delete, Regenerate cert, Admin API switch | SWITCH/ACTION | Out of scope | Immediate actions; outcomes in #setMsg / temp-credential display |
| MCP Servers — Add/Edit server (mcp.ts form-save) | FORM | Compliant (baseline, pinned in smoke) | Success closeForm + list repaint; failure keeps form, draft (wire state) intact, inline error |
| MCP Servers — credential Store on card; start-gateway credential prompt (mcp-prompt.ts) | ACTION (credential) | Excluded — previously approved security behavior | Value cleared on BOTH outcomes by security design ("never rendered back"); failure shows error, row stays resolvable. Store is not an object-editing form; changing it would violate that design |
| Optimize — Purpose editor (data-opt-purpose-save) | FORM | Compliant (baseline, pinned in smoke) | Success closes editor (openEditors.delete) + repaint with new purpose; failure keeps editor open, draft in drafts map, statusLine error |
| Optimize — Apply accepted / Apply destructive (data-opt-act=apply*) | ACTION | Not a form (classification recorded) | Plan execution: items redisplay in place with applied/failed outcomes + statusLine; the plan stays visible as the results surface. It does not edit one object in a form; it executes a reviewed plan whose rows are the object display, and they do redisplay in their new state. Closing/discarding the plan on apply would destroy the outcome record the Optimize workflow relies on — not changed |
| Optimize — Consult (directive), triage accept/deny, Measure/Estimate/Discover jobs | ACTION/IMM | Out of scope | Job submissions and immediate triage state changes with in-place outcome display |
| Activation Plan / activation banner — Apply plan, Drain+Apply, Apply now, Cancel changes | ACTION | Out of scope (not a form) | Executes the pending-changes plan; outcomes in place |
| Setup Wizard — step Run actions, Mark setup complete | Multi-step flow | Excluded by rule 17 itself | Advances/runs steps rather than closing |
| Logs — filter controls + date/time picker Apply | Filter/picker | Excluded | Picker Apply closes the picker and commits via the input's change event; filters apply immediately. Not object-editing forms |
| GPUs — telemetry interval preset select | IMM | Out of scope | Commits on change through setTelemetryInterval |
| Dashboard / Service Map / GPUs views | — | Nothing to audit | No save/apply forms present |

Harness honesty: ui/scripts/smoke.mjs is a static source-pin harness — it
cannot drive a live DOM save to success or failure. Per fixed or verified
surface the smoke pins assert the handler fragments that produce the
outcome (close/clear only in or after the success branch; failure branch
returns ahead of any close/repaint that would discard input), the same
pattern the pre-existing Add Model pins use. Runtime click-through
verification belongs to Pete's on-machine pass.

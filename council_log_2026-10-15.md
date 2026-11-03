# Council log — 2026-10-15

## 02:44 — Review of v0.1.2 backend wiring (src-tauri spec registration, list_backends/list_models extensions, withGlobalTauri)
- **Config:** panel review (Security Auditor, Correctness Hawk, Feasibility Analyst, Clarity Editor) · rounds 2 · research skipped — internal codebase, all facts code-checkable
- **Invoked by:** user (AFK session, per-section council reviews directed 2026-10-15)
- **Brief:** Ship/no-ship review of the v0.1.2 backend wiring change set: withGlobalTauri fix; backend_spec()/ensure_backend_specs() supervisor spec registration; list_backends supervisor-status merge; list_models on_disk/size_bytes. Single-user Windows desktop app; commands callable only from the app's own webview.
- **Evidence:** no research pack; orchestrator-verified facts F1–F10 from direct code reads (reap_exited has 0 callers; get_gpu_telemetry returns {samples, at_ms} while the UI reads .gpus/.stale; the telemetry Poller is never constructed in src-tauri; dashboard.ts calls .toFixed()/.toLocaleString() on fields list_backends never returns; UI stop/restart always graceful: true with a no-op Windows signal and a 30s drain under the supervisor Mutex; kill_on_drop(true) set on children; download_model writes models/<id>.gguf while list_models probes gguf_path; 312 workspace tests pass, src-tauri untested and outside the workspace).
- **Security Auditor:** 2 lows carried (raw_flags --host LAN bind; config-controlled program path) + 1 new low (operator policy — enabled flag and stale specs — enforced nowhere on the start path) (high conf) — config is not only attacker input but operator policy, and the command layer does not enforce it.
- **Correctness Hawk:** high — dead backend reports running forever; new highs — telemetry wrapper discards payload, dashboard render throws TypeError; mediums — enabled, 30s drain, partial downloads indistinguishable, stale specs, zero boundary tests; lows — raw_flags quoting, gguf destination mismatch (confirmed) (high conf).
- **Feasibility Analyst:** high — dead-backend blindness (fix is hours); adopted both UI contract findings as highs (monitoring surfaces fail silent); mediums — no preflight check with opaque error, 30s drain (certainty revised up), invisible renamed/removed running backends, disabled backend startable; version finding withdrawn (high conf).
- **Clarity Editor:** critical — BackendInfo/BackendRow contract break with no spec arbiter in the repo; highs — stale "stub" comments, telemetry mismatch, undocumented withGlobalTauri coupling; mediums — events "Reserved" comment, header "all wired" claim vs fit_check_available + dangling CONTRACT GAP pointer, model_file documentation, list_models doc (on_disk present-vs-complete ambiguity is load-bearing), ProcessState "Failed" unproducible; enabled revised to low (settled semantics, unrecorded) (high conf).
- **Round 2 movement:** heavy convergence — Feasibility adopted Clarity's two UI findings as highs; Correctness verified and adopted both, correcting the dashboard consequence upward (throws, not silently empty); Security widened raw_flags impact via the orphan/stale-spec composition and added the policy-enforcement low; Clarity settled enabled to Reading B and rebutted the version findings as defects; Feasibility withdrew its version finding.
- **Open questions (NEED):** whether anything upstream of dashboard poll() catches the render TypeError (blank vs stale frame only) — unresolved, immaterial to disposition.
- **Verdict:** DO NOT SHIP — confidence **MEDIUM** (rubric: one minor NEED open). Blocking: F4 dashboard TypeError, F1 dead-backend blindness, F2/F3 telemetry discarded + poller never started. Mitigated: Security's lows (single-user config trust + argv spawn/capabilities/CSP per F9); force-kill orphans (bounded by kill_on_drop, F6); version findings (scheduled bump step, F8).
- **Recommended action:** call reap_exited() on the list_backends status path, then run the same payload-alignment pass over api.ts for F2/F4, then re-review.
- **Would change this verdict:** a verified run showing a killed backend leaving "running", the dashboard painting real data without throwing, and GPU telemetry populating from a started Poller (or honestly reporting stale) — disposition then becomes SHIP WITH FIXES on the remaining mediums.

---

# S1 re-review (post-fix) — 2026-10-15, rounds 1

**Verdict: SHIP WITH FIXES (Judge, confidence HIGH)** — logged before acting.

All three original blockers verified closed by Correctness with code citations, corroborated across seats. Gates: 314 workspace tests pass; UI build + smoke (35 checks) pass.

## Blocking (must fix before ship)
1. [high, Correctness+Feasibility] state.downloads never cleared on completion/failure — first Models-view download wedges downloading=true forever; preflight then blocks Start; Delete/re-download refused. Reap terminal handles.
2. [medium, Feasibility] model_verify_checksum + delete_model_file resolve raw gguf_path against CWD, not data_dir — Verify fails on every default-config model; Delete orphans the real file.
3. [medium, Correctness] download_model TOCTOU — contains check and insert separated by an await; concurrent invokes corrupt the file and orphan a handle.
4. [medium, Correctness+Feasibility] telemetry_set_interval neither persists nor affects the running poller — persist + disclose restart-applies, or wire re-cadencing.
5. [medium, Clarity] dashboard gaugeRing draws a full (gateway) / empty (backend) arc when health is absent while the text shows "—" — neutralize the arc when the metric is absent.

## Non-blocking (fix or explicitly accept)
- [low, Security] prune/surface loops treat MCP specs (mcp-<id>) as stray backends — FIX (scope to backend-owned ids).
- [medium, Correctness] download_model has no in-use guard (re-download truncates a file a running backend has mmap'd) — FIX (reject when a running backend uses the model).
- [medium, Clarity] doc contradictions: types.ts temp_peak_c/power_limit_w comment; BACKEND_STATUS under the "Reserved" comment; list_backends/getBackends docs omit `configured` + orphan rows — FIX.
- [low, Clarity] orphan rows show both "not in config" and "disabled" chips; topology legend claims "edge width ∝ tok/s" (unimplemented) — FIX labels.
- [low, Correctness] start_backend not reap-aware (AlreadyRunning for a corpse) — FIX (reap before spawn).
- [low, Correctness+Feasibility] preflight downloads gate keys on raw PathBuf equality — FIX (compare resolved paths).
- [low, Correctness] unregister_spec leaves failed_ids/crash_counts/trackers/dead_rings — FIX (clean per-id state).
- [low, Correctness] stop() DrainTimeout reports "stopped" while the process may still hold its port — FIX (mark failed on DrainTimeout).
- [low, Feasibility] no crash auto-restart (restart_policy machinery uncalled) — ACCEPTED: manual recovery by design for v0.1.2; disclosed in the return briefing.
- [low, Feasibility] NVML source is a stub; shipped telemetry is nvidia-smi only — ACCEPTED with disclosure (the fallback chain works on the operator's machine).

## Judge's would-change-this-verdict
A traced run showing a completed Models-view download -> list_models downloading=false -> Start succeeds without restart, plus verify/delete resolving under data_dir, moves this toward SHIP once the remaining blocking items close.

---

# S2 — Backends view — 2026-10-15, rounds 1

**Verdict: SHIP WITH FIXES (Judge, confidence HIGH)** — logged before acting.

Foundation verified sound by all seats: payload shapes agree across view/api/Rust, action gating matches the command layer, preflight errors reach the user verbatim, server-side re-validation bounds every UI defect.

## Blocking
1. [high Correctness / low Security] Delegated click listener accumulates on the persistent #view root (wireBackends adds one per visit, no teardown; main.ts cleanup slot is only fed by mountDashboard). After N visits, one Restart click issues N restart_backend invokes — duplicate restarts succeed (extra stop/start cycle, double downtime). Fix with the dashboard teardown pattern.
2. [medium, Correctness+Feasibility] The view never polls — a backend killed externally or dying after Start shows "running" indefinitely until manual Refresh.
3. [medium, Feasibility] Disabled-backend loop cannot be completed in the app: preflight says "enable it before starting" but no view calls backend_get_config/backend_apply_config. Wire an enable action.

## Non-blocking
- [medium, Feasibility] backend_test_request exists with zero UI callers — wire a Test action (mandate: no unwired commands).
- [medium, Feasibility] Failed card carries no failure reason — surface a log tail or a Logs link scoped to the backend.
- [medium, Clarity] "Restarts" is the crash-backoff count (crash_delay has no production caller; reset per spawn) — reads 0 forever. Relabel or remove.
- [medium, Clarity] "not in config" chip is the only explanation of the orphan state — add one sentence; reconcile api.ts wording with main.rs.
- [low, 3 seats] Launch-command rendering joins argv with spaces (unquotable, un-copyable with real spaced paths) — quote elements containing spaces.
- [low, Correctness] argv cache is stale for the whole mount and caches failures as if they were argv — don't cache errors; refetch on toggle.
- [low, Clarity] Launch-command panel shows the NEXT spawn's argv for a running backend — label it as such.
- [low, Clarity] Model label shows relative path for configured rows, absolute for orphans — normalize (strip data-dir prefix for orphans).
- [low, Clarity+Feasibility] Two contradictory Stop rules; orphan in "starting" gets no action; Start offered during "stopping" — unify (Stop whenever a live pid exists in running/starting; no Start while stopping).
- [low, Feasibility] Runtime spawn/stop errors are Debug-formatted supervisor errors without remediation — friendlier mapping for PortInUse/AlreadyRunning.

---

# S3 — Models view — 2026-10-15, rounds 1

**Verdict: DO NOT SHIP (Judge, confidence HIGH)** — logged before acting.

All four seats converged on the core flow: download a model and know it is complete and intact does not work as documented on any axis. Surrounding engineering verified sound (atomic guards, fail-closed placeholders, closed SSRF surface, wizard path does verify).

## Blocking
1. [high, all seats] SHA-256 never verified on the Models-view download path or before use; verified_at never written by any command — the documented integrity control and the "sha256 verified" badge are decorative.
2. [high, Correctness+Feasibility+Clarity] Download outcome never reaches the view or any reader: no wait() call, no progress listener, no poll. Failures dropped unread by the reap path; cancelling a finished download prints "Download cancelled." for a succeeded download.
3. [high, all seats] Partial/truncated files indistinguishable from complete models (on_disk at partial size, startable); includes post-restart and pause-then-cancel (which also breaks cancel's documented contract). No partial state, no Resume.
4. [high, Feasibility + orchestrator-verified] Fresh install shows an empty Models view (default_config models: [], nothing seeds the catalog); non-catalog models cannot be added (download_spec_for hard-requires a catalog entry, ignores ModelConfig.source_url).
5. [medium, Feasibility] model_delete does not persist the config removal — deleted models reappear after restart.

## Non-blocking
- [medium, Feasibility] Cancel destroys all progress with no disclosure; pause control unreachable in the UI.
- [low, Security] Resume trusts the 206 range start / 416 without a size comparison (largely mitigated once completion verification runs).
- [low, Security] gguf_path escapes the data dir by design — accepted under the operator-trust premise.
- [low, Security] delete_model_file reference guard compares raw strings, not validate()'s normalized comparison.
- [low, Correctness+Clarity] Verify offered on unpinned models, surfacing the raw malformed-hash error; hash-source texts (catalog/pinned/config) disagree.
- [low, Correctness] model_assign: split length not validated vs GPU count; mutates in-memory config, returns persisted: false.
- [low, Correctness+Clarity] Pause/resume commands unwrapped in api.ts; Paused state unrepresentable in the view.
- [low, Correctness] Download double-click race leaves a spurious "already in flight" failure note.
- [low, Feasibility] No disk-space pre-check or catalog size disclosure on download_model (the wizard has both).
- [low, Clarity] Delete confirm copy unconditional while the command refuses for referenced models.

---

# S4 — GPUs / telemetry / placement — 2026-10-15, rounds 1

**Verdict: SHIP WITH FIXES (Judge, confidence HIGH)** — logged before acting.

The telemetry pipeline itself is verified sound (staleness model boundary-tested, peaks genuine, index-keyed attribution, propose arithmetic correct, security surface clean apart from one narrow low). The defects are exposure, refresh, and persistence layered on a correct core.

## Blocking
1. [high, Correctness+Feasibility] GPUs view loads once and never refreshes — first-poll race renders a false "empty GPU list" cause; left open, hours-old values show with no stale banner. Poll + surface data age.
2. [high, Feasibility] Placement unreachable from the UI (no wrappers; dashboard placement hardcoded []). Wire propose/apply end to end.
3. [high, Feasibility] Per-process VRAM attribution collected but exposed nowhere: ProcessSample has no GPU index (parser drops the association), backend_id hardcoded None, get_gpu_telemetry returns no processes. Carry GPU index through the parser, return processes, match PIDs to supervisor backends.
4. [medium, Correctness] placement_apply validates weaker than propose (no split-length check), neither persists, tasks left Running forever. Parity + persist + finish_task.
5. [medium, 3 seats] Poll interval control misrepresents current and effective cadence: never initialized from config; offers 2s/10s that collapse to 5s; no Off option. Offer only Off/1s/5s, initialize from config, confirm the effective value.
6. [medium, Feasibility] Telemetry failure indistinguishable from a GPU-less machine (silent missed ticks; success with zero samples; false empty-list message). Surface source errors distinctly.

## Non-blocking
- [medium, Correctness+Feasibility] propose uses stale/absent GPU data unchecked; projection carries no age or GPU-index list — label proposals with snapshot age + GPU indices; filter or flag stale.
- [medium, Feasibility] Fit verdict is weights-only (no KV cache/compute allowance) — label as such.
- [medium, Clarity] Stale banner overclaims (any-sample trigger), per-card stale gets only a dot that dot() renders as idle; at_ms discarded — show per-GPU staleness with age.
- [medium, Clarity] Telemetry source never named in the UI; manager-telemetry crate doc describes a label no view renders — surface the source per GPU.
- [medium, Clarity] Doc contradictions: telemetry_set_interval doc ("in-memory until save lands") vs its persisting body; gpuCard comment claims peak/limit absent from the payload (they are carried); placement docs describe nonexistent UI behaviour.
- [low, Security] nvidia-smi spawned by bare name every tick — resolve C:\Windows\System32\nvidia-smi.exe once at construction on Windows.
- [low, 3 seats] saveTelemetrySnapshot mistyped ({ok} vs {path}); the written path is never shown.
- [low, Feasibility] fan_pct / mem_util_pct collected but dropped by the mapping — render them.
- [low, Clarity] Card labeling: peak window unstated; "GB" labels GiB values while the process table uses MiB; power limit unlabeled; "UTILIZATION" caption heads the VRAM line; api.ts interval comment omits the restart caveat.
- [open assumption] nvidia-smi index order vs llama.cpp CUDA device order for --tensor-split is assumed, not established — disclose to Pete in the return briefing; applied splits should be verified on his hardware.

---

# S5 — Logs view — 2026-10-15, rounds 1

**Verdict: SHIP WITH FIXES (Judge, confidence HIGH)** — logged before acting.

Capture substrate verified sound by all seats: capture-time timestamps, real stream tagging, dead-ring retention, no injection surface, single-scope queries newest-first and crate-tested.

## Blocking
1. [high, Correctness+Feasibility] logs_query scope "all" fills the limit in sorted id order and silently omits whole processes (including crash output). The merge must apply the limit to the combined stream and signal omission.
2. [medium, Correctness+Clarity] "all" merge is not chronological (contradicts the documented newest-first contract) and lines carry no source — MCP output folds in unlabeled. Add per-line source attribution; merge chronologically; reconcile docs.
3. [high, Feasibility] Restarting a failed backend destroys its retained ring (spawn discards the dead ring); neither view warns. Archive the prior ring across respawn under a reachable scope.

## Non-blocking
- [medium] Logs view never refreshes; the log-line event is emitted nowhere — poll with a follow control.
- [medium] level/search implemented and crate-tested but unreachable from the view — wire controls.
- [medium] No path from a failed backend to its logs — pass the backend id through the route (dashboard + Backends links).
- [medium] diagnostics_export_bundle unwired — add wrapper + Settings button, labeled as containing raw unredacted logs.
- [medium, Clarity] api.ts getLogs comment narrows the scope contract vs main.rs; empty-state text covers four states with one misleading message — align and reword (state session-only retention).
- [low] No truncation signal — add a total/truncated indicator; note the view cap vs the 10,000-line ring.
- [low] Retained logs of a removed backend unreachable after it stops — include retained-ring ids in the picker.
- [low] Timestamps are time-of-day only — add date (title/attribute or fuller format).
- [low, Security] spawn_reader has no per-line byte bound — cap line length in the reader.
- [low, Security] Diagnostics bundle's raw unredacted logs — ACCEPTED (explicit operator action, local-only zip, credential-free config), with the export labeled.

---

# S6 — Gateway view — 2026-10-15, rounds 1

**Verdict: SHIP WITH FIXES (Judge, confidence HIGH)** — logged before acting.

Core verified sound: YAML generation single-sourced between the wizard write path and the preview; config carries no secret material (cred_ref names only; a crate test asserts no "sk-" in generated YAML); view rendering fully escaped; empty/error states honest; the wizard's install loop is a complete operable path with a real health probe.

## Blocking
1. [high, Security] LiteLLM is spawned with no `--host` argument and binds 0.0.0.0 by default; the generated YAML sets no master_key — any LAN peer can consume the local backends unauthenticated via :4000. GatewayConfig has no host/bind field, so the exposure cannot be narrowed by configuration. The gateway must bind loopback (and/or require auth) consistent with the rest of the app.
2. [high, Correctness+Feasibility] gateway_status measures no liveness (config port/group/rule counts only, under a "Gateway liveness summary" doc); the app never supervises or probes the gateway at runtime; the sidebar's green dot is painted from that payload. No state of this surface can report the gateway down. Liveness must come from a real probe, as the wizard's audit mode already performs.
3. [high, Feasibility] The operator cannot start/stop/restart the gateway from the app outside the setup wizard, and the Gateway view neither links nor mentions the working recovery (re-running the wizard's start-gateway step). A dead gateway is a dead end in this view.
4. [high Clarity / medium Correctness / low Feasibility] The "Request log" tile renders `${cfg.request_log_len} entries`, presenting a ring-capacity setting (default 100) as a count of recorded requests. The actual RoutingLog is capped at 50, holds only dry-run traces, and is empty on a fresh start.
5. [medium, Security+Clarity] The documented OpenRouter key control does not exist: no code reads the credential value or sets OPENROUTER_API_KEY in the spawn environment (the spawn env map is empty), so the YAML's os.environ/OPENROUTER_API_KEY reference is never satisfied and the fallback cannot authenticate. Either implement the injection or remove the claim and the armed fallback end to end.
6. [medium, Feasibility+Clarity] The Routing flow display presents declared config as effective routing: every member/fallback rendered verbatim while generation silently drops disabled backends and a disabled OpenRouter tier; GatewayConfigData drops the openrouter section entirely (tier enabled/credentialed/cap state invisible); per-group strategy chips give no hint LiteLLM receives one global routing_strategy. Display must match effective generation or be labeled as declared config.

## Non-blocking
- [medium, Correctness+Feasibility+Clarity] gateway_test_routing / trace_routing misstate their basis (least-busy degrades to a shuffle draw while the reason claims "lowest reported load"; assumed health and dry-run status undisclosed in the reason; RoutingDecision has no dry-run marker). Bounded today (no view wires either command); fix before or with any wiring.
- [medium, Clarity+Feasibility] Crate module docs and the generated YAML's own header describe a diff-preview/apply/regenerate lifecycle on the Gateway page that does not exist (diff_config_yaml has zero callers outside its crate; no code writes the gateway YAML after the wizard step). Reconcile docs and the shipped header with the implemented lifecycle.
- [medium Clarity / low Correctness] The YAML preview is presented as the config the gateway consumes, but it is regenerated from current in-memory config on every call while the running gateway reads the file the wizard wrote; after config changes they diverge with no label.
- [medium, Feasibility] The view never states the client endpoint or auth posture (base URL http://127.0.0.1:4000/v1, bind address, key requirement) — state it alongside the bind fix.
- [low, Correctness+Clarity] update_gateway_config mutates in-memory only, returns persisted:false, no save ever lands, doc implies one will; no api.ts wrapper. Doc must stop implying durability (or the command must persist).
- [low, Correctness] wireGateway caches the YAML fetch one-shot for the view's life, including failures.
- [low, Security] generate_config_yaml interpolates group names raw into a `#` comment; a newline in a group name injects YAML content. Operator-settable only; generator integrity defect.
- [low, Clarity] The preview prints `api_key: local` for every local deployment with no in-view explanation that it is the dummy key LiteLLM requires to be non-empty.

## Judge's would-change-this-verdict
Toward SHIP: a traced run showing live/down gateway states rendering distinctly in view and sidebar, LiteLLM listening on 127.0.0.1 only after a wizard start, an OpenRouter deployment authenticating via the spawn environment (or the tier's claims removed end to end), and the tile/routing displays matching effective generation. Toward DO NOT SHIP: the pinned LiteLLM binding LAN despite a loopback directive; generated YAML diverging from the wizard's install-path write; a credential value found in config, YAML, or logs.

---

# S7 — Settings view — 2026-10-15, rounds 1

**Verdict: SHIP WITH FIXES (Judge, confidence HIGH)** — logged before acting.

Read path verified sound: every field the view reads exists in the real AppConfig serialization, rendering fully escaped, no secret values in the config schema, saves that occur are validated and atomic with .bak fallback, unsupported update checking stated honestly. Orchestrator-verified and accepted by the Judge: nothing in src-tauri or manager-mcp consumes cfg.mxc.mode for enforcement today (recorded config; external MXC runtime is the presumed consumer), which bounds the defects below to misreported state rather than a live enforcement gap.

## Blocking
1. [high, all four seats] mxc_set_mode does not persist: mutates only the in-memory config, never calls manager_config::save; the view's success message and repaint (re-reading the same in-memory value) present the change as durable. On restart the mode silently reverts to the on-disk value; alternatively an unrelated whole-config save (telemetry_set_interval, update_config) silently sweeps the unconfirmed in-memory change to disk. The mode control must save through manager_config::save and report the persisted result truthfully.
2. [high, Security] update_mxc_policy ignores the validator's verdict: validate() returns Ok(ValidationReport) with valid:false for bad policies (default_deny:false, wildcard network Allow "*", relative paths, secret-shaped credential strings) and the command writes the policy file unconditionally. It must refuse to write when valid is false. No UI wrapper calls it today; the stored file is what current_mxc_policy loads.
3. [medium, Correctness+Feasibility+Clarity] The "Active policy" card never renders the policy: settingsPaint summarizes the {policy, source, mode} envelope ("policy → { N fields }") and zero rules; a missing policy file's silent built-in-default substitution shows only as an unexplained "source: default". Unwrap .policy, render its actual rules, explain the default-substitution state.
4. [medium, Feasibility+Clarity] The MXC mode buttons carry no definitions anywhere in the UI (Learning = observe/log only; Enforced = blocks out-of-policy execution; Disabled = sandbox off), and mxc_run_self_test (harmless, pure policy evaluation) has no wrapper or button — the operator's first real test of Enforced is production tool execution. State mode consequences in the view and wire the self-test.

## Non-blocking
- [medium, Security] update_config installs a validation-rejected config in memory and returns ok:true with persisted:false — install in memory only on successful save. No UI caller today; on-disk stays last-known-good.
- [medium, Feasibility] diagnostics_export_bundle unreachable from the UI — wiring it into the Settings diagnostics card with an unredacted-contents warning is already mandated under S5; land it there.
- [medium, Feasibility] Everything else in Settings is display-only while write commands exist unwired (update_config, update_mxc_policy, winml_*); telemetry retention, notifications flag, per-component update pins, and the MXC policy cannot be changed in the app; WinML renders as one enabled/disabled line (no port, model_id, status, toggle).
- [medium, Clarity] "Policy unavailable." / "No recent MXC activity reported." conflate fetch failure with genuine absence — wireSettings never reads polR.error/actR.error.
- [medium, Clarity] The "Updates & diagnostics" card collapses updates.components into one "{ N fields }" row and contains no diagnostics content or action.
- [medium, Security] Switching MXC to Disabled requires no confirmation at either layer — add a confirmation alongside the persistence fix.
- [low, Security] mxc.path unconstrained (read-any-JSON / write-policy-anywhere pair for webview callers) — bounded by the threat model (operator owns config.json).
- [low, Security] Config validate() never screens cred_ref / source_url for secret-shaped values — a real key pasted into cred_ref would persist in plaintext and be copied into diagnostics bundles. Operator error required.
- [low, Feasibility] Config file path never shown (only the data directory); state.config_path is never returned by any command.
- [low, Feasibility] Diagnostics bundle versions file hardcodes app version "0.1.0" while the app ships as 0.1.2.
- [low, Clarity] "Re-run audit" button only navigates to the wizard (second click required) and leads to the setup-checklist audit.
- [low, Clarity] "Recent activity" entries render as truncated JSON.stringify rather than a readable account of decisions.
- [low, Clarity] Missing cfg.mxc/mode is indistinguishable from a real state (no Mode row, no highlight, buttons still clickable).
- [low, Clarity] Stale docs: update_mxc_policy comment ("live when manager-mxc lands"); api.ts mode comment drops the durability caveat (subsumed by blocking fix 1).
- [low, Clarity] Telemetry interval probing hides a missing row silently; sibling rows labeled with raw snake_case keys.
- [low, Correctness] wireSettings root-listener accumulation — already fixed under the S2/S3 teardown work.

## Judge's would-change-this-verdict
Toward SHIP: a traced run showing mxc_set_mode persisting with the mode retained across restart, update_mxc_policy refusing a default_deny:false policy without writing, and the Settings view rendering actual policy rules with the three modes defined. Toward DO NOT SHIP: a Settings-reachable path persisting a validator-rejected policy/config; on-disk config corruption from any of these flows; or evidence an external MXC runtime consumes cfg.mxc.mode for live enforcement in this build.

---

# S8 — Service Map + Dashboard — 2026-10-15, rounds 1

**Verdict: SHIP WITH FIXES (Judge, confidence HIGH)** — logged before acting.

The live data path under both surfaces is verified honest by all seats: no live figure is fabricated (throughput chart omitted rather than invented, absent metrics render "—" behind a neutral ring track, backend states map 1:1 from reaped supervisor state, crashed vs never-started distinguished, Restart correctly scoped with real failure surfacing). The defects are concentrated in the presentation layer. Two root causes (gateway probe, placement derivation) are owned by the S6 and S4 batches.

## Blocking
1. [high, Correctness+Feasibility+Clarity] Gateway state is asserted, not measured, on every S8 surface: the Service Map gateway node and the sidebar "LiteLLM" row go green whenever the Tauri invokes succeed; the sidebar ignores gw.error and still paints the fallback port; the dashboard's gateway card can never appear live (getDashboard searches list_backends for a row that never exists). Once S6's probe lands, all three surfaces must key gateway state to the probe result.
2. [medium, Correctness+Feasibility+Clarity] The header verdicts contradict each other: the dashboard pill reads "All systems normal" when every backend is stopped and even when nothing is configured (stopped → "idle", not severe), while the map's pill correctly reports "N / M backends running" / "No backends configured"; with telemetry stale the two invert again. One verdict rule on both surfaces; an all-stopped or unconfigured stack must not read as normal.
3. [medium, Correctness+Feasibility+Clarity] The Service Map paints exactly once — no poll, no refresh control, no timestamp — under a "live data" header label. Refresh on the dashboard's pattern, or label honestly as a snapshot with its fetch time.
4. [medium, Correctness+Feasibility+Clarity] The "Model placement" card renders as a titled, permanently empty card on every live install (no rows, legend, or empty state). Resolves by populating from S4's tensor_split derivation; until then it must carry an explicit empty state.
5. [medium Correctness+Clarity / low Feasibility] The dashboard Topology mini-map misstates status under the title "Topology — same data, node view": hardcoded "Continue.dev"/"Cline" client nodes with green ("healthy") dots no data source reports; LiteLLM node green unconditionally with a hardcoded ":4000 · least-busy" sublabel; GPU dots always green regardless of g.status; every backend→GPU edge class "hot" regardless of backend state; a failed backend shown amber, which the legend defines only as "degraded / starting". Remove or mark hardcoded nodes as illustrative; derive dots/edges from the same status data as the other modes.
6. [medium, Correctness+Feasibility+Clarity] Service Map backend→GPU edges are a positional full mesh styled by backend state regardless of actual tensor_split/split_mode; the payload reports no GPU assignment and nothing in the UI discloses that. Edges must reflect reported assignment (available once S4 derives placement) or the mesh must be labeled as not representing per-backend assignment; backend nodes should render the model_file already in the payload.
7. [medium Clarity / low Correctness+Feasibility] The map hardcodes "MXC · LEARNING-MODE" (boundary label) and "BACKENDS · POLICY-CHECKED" (column header) regardless of the configured MXC mode — including Disabled, where the policy-checked claim is false — while the sidebar prints the real mode. Both labels must derive from cfg.mxc.mode; the policy-checked claim must not appear when the mode does not check.

## Non-blocking
- [medium, Feasibility] No Service Map node is actionable (no click handlers, no link to Backends/Logs) — an "Attention needed" pill gives no next step.
- [medium Clarity / low Correctness+Feasibility] Sparkline captions ("Tokens/s · 1h", "Requests per minute", Console "Trend") head permanently empty areas on the live path — omit or mark uninstrumented when no series exists.
- [low, Security] charts.ts helpers are unescaped innerHTML sinks (stackedBar segment label into an SVG <title>; gaugeRing centerTop/centerBottom) — not reachable with untrusted data today, but the stackedBar sink becomes live when S4 wires placement to config data; the helpers should esc() their inputs, landed with or before that wiring.
- [low, Correctness] Dashboard poll teardown race: an in-flight doPoll at navigation renders dashboard HTML into the new route's #view; dashboard.ts has no mounted/generation guard for in-flight polls.
- [low, Correctness+Clarity] The dashboard's stated STALE rule is not applied while paused; mitigated by the "Last updated Xs ago" ticker.
- [low, Correctness+Clarity] Service Map cluster summary "Backend restarts" sums the crash-backoff counter (dormant while 0; same mislabel removed from the Backends card under S2).
- [low, Clarity] Dead-code fabrication path: static.ts exports throughputSeriesFor (walk() random-walk series, no caller) — remove it or quarantine it to mock.ts. The sample-mode throughput chart title claims a measured "1h" window the walk() data does not model.
- [low, Clarity] Label collisions: map "VRAM held" is all-process telemetry VRAM used vs dashboard "VRAM HELD" undefined → "—"; map GPU dot colors have no legend entries; "degraded" means both a stopping backend and a stale GPU sample.
- [low, Feasibility] Unrecognized supervisor states render on the map with the grey "stopped" appearance.
- [low, Feasibility] Dashboard Logs button carries the backend id but navigates to bare #/logs — use the hash-param scope routing S5's batch adds.
- [low, Correctness] getDashboard's gateway heuristic (id "gateway" or port 4000 reclassified as the gateway) is latent mislabeling if an operator configures a backend on port 4000.
- [low, Clarity] Gateway service-card ring draws its arc at a fixed 100 whenever requests_1h is present (sample mode only) — the fill encodes nothing.

## Judge's would-change-this-verdict
Toward SHIP: a traced run with LiteLLM killed showing the map node, sidebar row, and dashboard gateway presence all rendering down from S6's probe; both header pills applying one verdict rule across stopped/stale/unconfigured states; the map refreshing on an interval or labeled as a timestamped snapshot; the placement card populated from S4's derivation (or explicit empty state); mini-map dots/edges/sublabels derived from the same status data as the other modes. Toward DO NOT SHIP: any live-path figure fabricated (throughputSeriesFor or mock placement reaching the live dashboard), or gateway indicators still keyed to invoke success after S6's probe exists.

---

# S2 re-review (post-fix) — 2026-10-15, rounds 1

**Verdict: SHIP WITH FIXES (Judge, confidence HIGH)** — logged before acting.

All three original blocking items verified closed by every seat with code evidence: the delegated listener is a single named handler removed by a teardown main.ts captures per route; the view polls every 5 s with action-in-flight skips; the enable/disable loop completes in-view through applyBackendConfig. The original non-blocking items are likewise closed (test wiring honoring the non-throwing TestResult contract, per-episode log tails, Restarts row removed, argv quoting and "next start" caption, unified Stop rule, orphan gating and explanation).

## Blocking
1. [medium, Feasibility+Clarity] Enable/Disable is not durable: backend_apply_config mutates only the in-memory config, never calls manager_config::save, hardcodes "persisted": false, and the view ignores the flag — on restart the enabled flag silently reverts (or is swept to disk by an unrelated save). Same defect class as S7's mxc_set_mode blocking item; fix with the same pattern (save through manager_config::save, report persisted truthfully). Scheduled for the wave-2 Rust batch.

## Non-blocking
- [medium Clarity / low Correctness+Security] Supervisor errors still reach the card in Rust Debug format ("PortInUse(8081)") although SupervisorError has plain Display text no layer uses — fix via Display in ApiError::from (wave-2); does not gate the release (error reaches the user verbatim, escaped, actionable).
- [low, Feasibility] "Open in Logs" link lacks scope — already being fixed by the in-flight S5 batch (hash-param routing).
- [low, Correctness] Orphan model_file shows the spec's absolute path vs configured rows' relative path — strip the data-dir prefix for display parity.
- [low, Correctness+Clarity] Orphan note asserts "Still running" unconditionally — make it state-aware.
- [low, Correctness] A clicked button's disabled state does not survive a repaint — a second click in the in-flight window can issue a duplicate invoke; move the guard outside the repainted DOM.
- [low, Correctness] msgs map never cleared on state change; load() has no in-flight guard (overlapping loads race; worst case one stale paint cycle).
- [low, Security] argv/test branches of onClick call paint() after awaits without the disposed check — make the guard uniform.
- [low, Clarity] Failed card's log tail has no caption (most recent lines, stderr-preferred, from up to 50 records).

---

# S3 re-review (post-fix) — 2026-10-15, rounds 1

**Verdict: SHIP WITH FIXES (Judge, confidence HIGH)** — logged before acting. The original DO NOT SHIP is lifted: all four seats verified every one of the five original blocking findings closed in code (watcher verification with verified_at stamped only after a real comparison, placeholders fail closed on every path; outcomes reach the view; .part discipline incl. post-restart partials; catalog seeding + source_url fallback; model_delete persists with the refusal contract). Gates agree (317 workspace tests, src-tauri checks, UI build + smoke).

## Blocking
1. [medium, Security] Verification is not strictly ordered before use: the transfer handle is reaped at rename while the watcher's hash runs afterward (seconds for multi-GB files); in that window the file shows complete and passes start preflight unverified, and on mismatch the watcher deletes the file — possibly under a just-started backend — with the delete result ignored, so a failed delete leaves the mismatched file serving with no error surfaced. Start must be gated until the watcher resolves (or the row must show a verifying state), and a failed mismatch-delete must be recorded and surfaced.
2. [medium, Correctness+Feasibility] Delete-all is undone by the seeder: deleting the final model persists an empty list; the next list_models re-seeds the full catalog, contradicting both the model_delete persistence fix and the seeder's own doc claim. Seeding needs a once-only marker distinct from "models is empty".
3. [medium, Clarity; converged by Security at low] The verified_at badge is stale-prone: it renders whenever verified_at is set, independent of on_disk or a later failed verification; download_model never clears it at attempt start and the watcher's mismatch branch deletes the file without clearing it. Clear verified_at in the mismatch branch and at download start, and/or gate the badge on on_disk.

## Non-blocking
- [low, Security+Correctness] A verify_sha256 I/O error records "verification failed" but leaves the unverified file in place and startable; disclosed via download_error and judged defensible (a transient I/O error should not delete the file) — the start path should keep the unverified state distinct from a clean completion.
- [low, Correctness] model_assign (and placement_apply) record_task without finish_task — owned by the in-flight S4 batch; this verdict assumes that batch lands them.
- [low, Correctness] The 206 start-mismatch and stale-416 recovery branches are implemented but untested — add branch tests with the wave-2 fixes.
- [low, Correctness] Stale watcher outcome race: download_outcomes keyed by model id with no attempt generation (cosmetic today; the view suppresses the error while downloading).
- [low, Feasibility] Seeded rows display "Parameters: 0B" (params_b hardcoded 0.0) — show the field as unknown rather than 0B.
- [low, Feasibility] Spawn-window Pause/Cancel buttons are live before the handle exists — keep them inert until the spawn invoke settles.
- [low, Clarity] The UI and backend "pinned" tests differ (UI length ≥64 + no "PLACEHOLDER"; backend length ==64 + all hex) — consistency cleanup; the layers agree on real pins and catalog placeholders.

## Judge's would-change-this-verdict
Toward SHIP: a traced run showing a start in the post-rename window gated until the watcher resolves (or a verifying row state), a mismatch against a running backend surfacing an error instead of an ignored delete, delete-all leaving the catalog empty across a restart, and a mismatch clearing verified_at — with the model_assign/placement_apply task completions landed by the S4 batch. Toward DO NOT SHIP: evidence the unverified window is hit in the normal flow (e.g. an app exit between rename and watcher completion leaving a complete, startable, never-re-verified file with no signal on next launch), or a mismatch delete silently failing on Windows while the bad file keeps serving.

---

# S5 re-review (post-fix) — 2026-10-15, rounds 1

**Verdict: SHIP WITH FIXES (Judge, confidence HIGH)** — logged before acting.

Both original blocking items verified closed by all four seats with code evidence and re-run tests: the "all" merge is chronological, attributed (source + archived on every record), counted before truncation, and MCP scopes are reachable; restart preserves the previous run as one archived generation with a verified memory bound (three fixed-capacity rings per id, no accumulation path). Every original non-blocking item is also closed (Follow tail, level/search, deep links, diagnostics export with the accepted unredacted-logs warning in place, split empty states, full-date timestamps, 64 KiB line cap).

## Blocking
1. [medium, Clarity; low Feasibility] The one-generation archive limit is disclosed nowhere: a second restart silently replaces the archived generation and discards the first run's logs. Add the disclosure to the Logs meta line ("…up to 10,000 lines per process; one previous run per process is kept.").
2. [medium, Clarity; low Feasibility+Correctness] The picker previous-run label never reaches its primary case and misdescribes the scope where it renders: mergeScopeOptions never updates existing option labels (configured backends keep bare labels all session), and "(previous run)" is false whenever a current ring exists since the query returns current + archived merged. Adopt "(incl. previous run)", refresh existing option labels when merging payload scopes, and align the main.rs docstring and LogScope doc phrasing.

## Non-blocking
- [low, Feasibility+Clarity] The Level filter's heuristic nature (token matching) is not disclosed in the view.
- [low, Feasibility] diagnostics_export_bundle enumerates spec_ids() rather than log_scopes() (unregistered processes' logs absent) and writes archived records with no previous-run marker.
- [low, Clarity] The truncation line does not cover ring eviction (total_matching counts retained records only).
- [low, Correctness] The log-line event surface is still dead while the module header claims the primary streams are emitted below — move it to the reserved block or correct the header.
- [low, Security] The truncation marker is forgeable in-band by process output — judged negligible (no trust boundary crossed); accepted, no change.

---

# S4 re-review (post-fix) — 2026-10-15, rounds 1

**Verdict: SHIP WITH FIXES (Judge, confidence HIGH)** — logged before acting.

The mechanism layer is verified closed by all four seats: 2 s poll with correct teardown, gpu_index attribution proven against minor_number ordering, PID→backend matching after reap, identical cadence buckets on both sides of the wire, poll-error recording, placement_apply validate-before-save with truthful persisted reporting and task completion, and all 14 original clarity items. Not DO NOT SHIP: the placement flow fails closed with accurate errors, nothing is corrupted or misreported as saved, and telemetry (the section's primary surface) is sound.

## Blocking
1. [high, Feasibility] Placement cannot be completed end to end from the UI: (1) default-config backends (planner.gguf, coder.gguf, coder-fast.gguf, tool-runner.gguf) match none of the view's model-match rules against the seeded catalog models, so every Placement option renders disabled; (2) placement_apply errors "use model_assign first" whenever assigned_backends is empty, and no production path populates it (seeding writes Vec::new(), the wizard writes it only in test fixtures, no UI view or api.ts wrapper calls model_assign), so Apply always fails on any app-produced configuration. The flow must be completable from the UI and selectable for the configurations the app itself creates.
2. [medium, Feasibility] placement_propose resolves models solely via curated_models() and returns NotFound for config-only models (e.g. source_url models made downloadable by the S3 work) that the Placement select affirmatively enables. Propose must resolve config models, or the select must not offer models it cannot propose for.
3. [medium, Clarity; low Correctness+Feasibility] The GPUs header data age is stamped at fetch time, not snapshot time: get_gpu_telemetry sets at_ms to now_ms() at serve time instead of the store's last_successful_poll_at() (which placement_propose already uses correctly), so under the 2 s view poll the header always reads "updated just now" — including while the failing-source banner above it reports last-received samples from a source possibly down for minutes. Anchor at_ms to the last successful poll (or the newest sample timestamp).

## Non-blocking
- [low, Correctness+Feasibility] First-load false empty state narrowed, not eliminated (no "no poll completed yet" signal in the payload; up to one 2 s cycle on a healthy machine).
- [low, Security] The wizard's gpu_inventory still spawns bare-name nvidia-smi (ops.rs, steps.rs) — share the System32-first resolution across spawn sites.
- [low, Security+Correctness] placement_apply can leave memory and disk diverged on a late validate/save failure.
- [low, Correctness] The telemetry store never prunes vanished GPUs (a removed GPU stays in snapshot()/gpu_count() and counts toward split-length validation).
- [low, Correctness] Placement error phrasing not uniform between propose and apply/model_assign pre-checks; apply reports a nonexistent model as "not assigned" rather than NotFound.
- [low, Clarity] dot() has no "degraded" case in the GPUs view (dashboard maps degraded to amber; the stale chip carries the signal here).
- [low, Clarity] placement_apply's doc claims "the UI dispatches that via restart_backend" — the Apply handler never calls it; the user-facing copy is truthful, the doc is not.
- [low, Clarity+Feasibility] The two poll scopes are not distinguishable to the reader ("Poll" sets the sampler cadence; the view silently re-fetches every 2 s; "Off" still performs one initial poll at startup).
- [low, Clarity] The "peak N °C since app start" label goes stale after Reset peaks.
- [low, Clarity+Feasibility] The projection's Free column is recomputed client-side from a different telemetry basis than the server projection's fits verdict.
- [low, Feasibility] Placement form data is fetched once at mount; changes elsewhere do not appear until remount.

---

## S6 Gateway — re-review verdict (Judge, 2026-10-15 ~08:02 EDT)

**VERDICT: SHIP WITH FIXES** (confidence: high), on the four seats' code-level verification.

The fix batch closed the large majority of the original items, all verified at code level: loopback-only bind on the single production spawn path, YAML comment-injection sanitization with a regression test, a loopback-hardcoded timeout-bounded probe, truthful clone → save → install persistence for gateway config edits, honest declared-vs-effective routing disclosure, and no credential material in the YAML, planned commands, or logs. Two defects remain that silently defeat behavior the UI asserts: the OpenRouter key is dropped before the production spawn, so the fallback the app reports as armed is never armed; and Restart while the gateway is up reports success while the old process keeps serving the old generated config, with no stop path at all. Neither is a leak or an exposure, and both have known, scoped fixes.

### Blocking
- [high — Security] `RealSystemOps::spawn_detached` builds the Command from argv only and never applies `PlannedCommand.env`, so `OPENROUTER_API_KEY` is silently dropped in production and the YAML's `os.environ` reference is unsatisfied. FakeSystemOps records `cmd.env` verbatim, so the injection test passes while the real binary drops the variable. Fix: the same env loop `run_command` uses, plus coverage exercising the real spawn path's env delivery.
- [medium — Feasibility] No gateway stop, and "Restart gateway" while up does not restart: the spawned process is untracked (no pid, no kill path), the step spawns a second LiteLLM that fails to bind and exits, and `wait_healthy` is satisfied by the old process, so the step reports success while the old process serves the old config — silently defeating the YAML card's "until the gateway is restarted" disclosure. Fix: record the spawned pid and implement stop/restart semantics that terminate the old process before spawning.

### Non-blocking (lows, accepted or wave-3 cleanup)
- decode_credential_blob's UTF-16LE-first heuristic can decode an even-length UTF-8 blob as garbage while the step reports the fallback armed (Security).
- The probe counts only 2xx as up; any syntactically valid HTTP status line is the more precise liveness semantic (Correctness; not reachable in app-produced configs).
- Sidebar gateway row refreshes only on navigation (Feasibility).
- The fallback's armed state is not persistently visible on any surface (Feasibility).
- A fallback naming a fully-dropped group renders unmarked in the Routing flow (Feasibility).
- wireGateway's load() ignores the getBackends() error; exclusion chips silently disappear on failure (Feasibility).
- manager-gateway docs still claim pick_backend enforces per-group strategies in production; it has no production caller outside trace_routing (Clarity; developer-doc level).
- The Start button's permanent note states the fallback is armed unconditionally; it should be conditional on the tier being enabled (Clarity).
- The CredReadW block is statically sound but cfg(windows) and never compiled; the Windows release build is its first real compile and must be watched as a release gate (Security/orchestrator note).

### Would change the verdict
To SHIP: wave-3 evidence that spawn_detached applies env with a real-path test, a recorded pid with a working stop and a restart-while-up that demonstrably replaces the process and serves the regenerated config, and a Windows release build compiling the CredReadW block. To DO NOT SHIP: the key reaching logs/YAML/planned output anywhere, a loopback/probe regression, or the env/restart defects proving structural.

---

## S8 Service Map + Dashboard — re-review verdict (Judge, 2026-10-15 ~08:02 EDT)

**VERDICT: SHIP WITH FIXES** (confidence: high), on the four seats' code-level verification.

Six of the seven original blocking items are verified closed by every seat that owned them: gateway state is probe-measured on the map, sidebar, and dashboard; both surfaces share one `systemVerdict` with equivalent inputs; the map polls with proper teardown and draws edges only from configured tensor splits; the permanently empty placement card and the fabricated mini-map content are gone; all config-originated strings pass `esc()`. One original blocking item remains open at high severity: model placement cannot be completed end to end from the UI on an app-produced configuration. The command layer is fixed and verified, but the GPUs view's filename-match gate dead-ends first, and `model_assign` — the one command that aligns a backend to a model — has no UI caller. That is a single, well-scoped UI fix with a determined shape (operator-stated backend/model selection, model_assign wired in, then propose/apply), fitting SHIP WITH FIXES for the wave-3 micro-batch. Everything else is low severity.

### Blocking
- [high — Feasibility] Placement cannot be completed end to end from the UI on an app-produced config: `default_config` writes `model_file` as `models/{backend-id}.gguf` while catalog models carry Hugging Face `gguf_path` basenames; none of the view's three filename-match rules can align them (and `baseName` keeps the extension, so the id rule compares an id against "planner.gguf"), so every Placement option renders disabled "(no matching model entry)" and Propose/Apply are unreachable; the wizard never writes `model_file` and `model_assign` has no UI caller.

### Non-blocking (lows; wave-3 cleanup or accepted)
- Mini-map MODELS nodes assert "loaded" with an unconditional green dot from configuration alone (all four seats); mini-map backend→GPU edges remain a positional full mesh with positional model↔GPU pairing.
- `splitsFromConfig` filters non-numeric entries, shifting later fractions one GPU index earlier than `placementFromConfig` (hand-edited configs only).
- `mountDashboard` never resets the module-level `paused` flag; returning to a paused dashboard can show a blank view for up to ~10 s.
- `wireServiceMap` never reads `cfg.error` (split edges silently vanish, bare "MXC" fallback) and renders a zeroed gateway fallback on `gw.error` instead of an explicit unknown state.
- `getDashboard` joins any partial error (including a gateway-only probe failure) into its result error, freezing the dashboard at the last fully-successful model while the map repaints fresh data behind the same verdict text.
- The sidebar LiteLLM row does not poll (converges with S6).
- Dashboard placement segment colors/legend are keyed to sample GPU ids, so live segments fall back to grey and the legend shows colors that never appear.
- Map node links are legend-level only, not per-node (original item met in weakened form).
- Map poll worst case ~10 s when the in-flight guard drops a tick.
- "BACKENDS · POLICY-CHECKED" is claimed when the MXC mode is unknown (null), not only when known-and-not-disabled.
- The map asserts "No backends configured" in sample mode (disagreeing with the dashboard mock pill) and on live `list_backends` failure (while its own banner discloses the error).
- `systemVerdict` masks a down gateway in partial/all-stopped verdicts (gateway state shown on node/card, not in the pill).
- Probe authenticity bound assessed acceptable with no change (loopback-only, no credential, no security boundary crossed).

### Would change the verdict
To SHIP: end-to-end placement from the UI on an unmodified app-produced default config (model_assign wired into the Placement flow or the filename-match gate replaced with operator-stated assignment; Propose and Apply reachable; the resulting tensor_split persisted), gates still green. To DO NOT SHIP: the placement command layer also failing on app-produced configs, any of the other six original blockers regressing, or the UI gate proving unfixable without changing the verified command-layer contracts.

---

## S7 Settings — re-review verdict (Judge, 2026-10-15 ~08:02 EDT)

**VERDICT: SHIP WITH FIXES** (confidence: high), on the four seats' code-level verification.

All four original blocking items are unanimously verified closed: the `update_mxc_policy` validator gate refuses before any write with redacted errors and no bypass path (the wizard's only other policy write is the validated code-constant default, and writes nothing in Audit mode); `mxc_set_mode` and `update_config` use clone → save → install with truthful persisted reporting; the diagnostics bundle carries the real version, `log_scopes`-enumerated scopes, and `[archived]` tags with the accepted unredacted warning intact. The standing absence of any Tauri-layer MXC enforcement consumer bounds every remaining Settings defect to misreported state. The batch is not an unconditional SHIP because one new medium retains blocking item 1's mechanism in two backend commands and one low leaves the last ok:true-on-save-failure payload in place; both are small, pattern-established fixes for the wave-3 micro-batch, and neither is reachable as a false user-facing claim in the current UI.

### Blocking
- [medium — Correctness] `updates_pin` and `updates_set_auto` mutate `cfg.updates.components` in memory and return `{ok: true}` with no `persisted` field and no save anywhere, so a pin or auto-update toggle silently reverts on restart or is swept by an unrelated save — blocking item 1's defect class intact. Fix with clone → save → install and a truthful persisted field before any UI wiring, or remove/disable the commands.
- [low — Correctness] `setup_mark_complete` sets `cfg.setup_completed = true` in memory before saving and returns `Ok({ok: true, persisted: false, error})` on save failure with the flag left set. Convert to clone → save → install and return a proper error on save failure so no command answers a failed save with an ok:true payload.

### Non-blocking (lows; wave-3 cleanup or accepted)
- Two unreconciled default MXC policy locations (wizard `<data_dir>/mxc/policy.json` vs AppConfig default `<data_dir>/mxc-policy.json`); align the defaults (Security).
- `update_mxc_policy` writes non-atomically (plain `fs::write`); tmp+rename (Security).
- A residual group (telemetry_set_interval, model_assign, model_delete, model_verify_checksum, watcher stamps) mutates in place then saves without rollback; callers get truthful errors — consistency cleanup, not a live defect (Correctness).
- update_config / update_mxc_policy / winml_* remain UI-unwired with no edit affordance implied — honest display-only remainder, acceptable for v0.1.2 (Feasibility).
- Failed self-test cases state no fix path; add the hand-edit guidance (Feasibility).
- Sample-mode setMxcMode returns ok:true with persisted undefined; return an explicit not-available error (Feasibility).
- MXC mode definitions never name the consumer; add the one-line disclosure (Clarity).
- The Updates card prints "Nothing reported." above a populated Components section; suppress the placeholder when components exist (Clarity).

### Would change the verdict
To SHIP: wave-3 evidence that updates_pin/updates_set_auto persist via clone → save → install with a truthful persisted field (or are removed) and setup_mark_complete no longer mutates before save or reports ok:true on failure, gates green. To DO NOT SHIP: a shipped UI or production caller invoking the two commands or relying on setup_mark_complete's in-memory flag, a Tauri-layer consumer of manager_mxc::decide making the dual paths live enforcement defects, or a regression in the validator gate / clone → save → install commands.

---

## Wave 3 — verification of the re-review blocking fixes (2026-10-15 ~08:19 EDT)

Three targeted verification seats re-checked the wave-3 fixes adversarially against production code. All VERIFIED, confidence high:

- **S6 env delivery**: the env loop is inside `RealSystemOps::spawn_detached` itself (ops.rs:262); the regression test drives RealSystemOps (not the fake) and fails if the loop is removed; steps.rs:1314 is the only production spawn caller.
- **S6 gateway stop/restart**: pid file written in Install mode only; stop-before-spawn fails the step rather than spawning a duplicate; `gateway_stop` registered in the invoke handler; `pid_is_signalable` (1..=i32::MAX) guards both the alive and kill paths with a regression test (a corrupt pid file can no longer broadcast a signal — a hazard the wave-3 agent caught firing during its own testing, along with a process-group kill form that terminated the wrong group and was removed); Windows kill is `taskkill /T /F` (tree), Unix kills the bare pid.
- **S7 persistence**: `updates_pin`/`updates_set_auto` and `setup_mark_complete` verified clone → save → install; a full-file sweep of every `manager_config::save` call site confirms no command in main.rs answers a save failure with an ok:true payload anymore.
- **S8 placement**: end-to-end trace on the unmodified default config — all 4 backends and 4 seeded models selectable, Propose independent of assignment, Apply sequences model_assign → placement_apply and persists the split; the filename-match gate and "(no matching model entry)" text are gone.

Merged-tree gates after wave 3: 345 workspace tests pass (0 failed), workspace clippy 0 warnings, fmt clean, src-tauri check + check --tests clean, UI build + smoke (46 checks) pass. Remaining release-gate item per the S6 judge: the Windows release build is the first real compile of the cfg(windows) CredReadW block and the tasklist/taskkill shell-outs.

**Council status: all 8 sections reviewed, fixed, re-reviewed (SHIP WITH FIXES), and the re-review blocking items fixed and verified. Proceeding to release v0.1.2 under the departure default-YES.**

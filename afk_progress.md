# AFK progress — Local LLM Service Manager v0.1.2

- 02:31 EDT — Implementation complete: withGlobalTauri fix; backend spec registration (backend_spec/ensure_backend_specs in src-tauri main.rs); list_backends live state merge; list_models on_disk/size_bytes; api.ts wrappers for all views; all six static views wired (subagent); Service Map + Dashboard restart wired (subagent); main.ts integration incl. sidebar footer live MXC mode.
- 02:33 EDT — Gates green: UI build + smoke (26 command guards, no-mock-banner guards); cargo fmt/clippy clean (workspace); 312 workspace tests pass; src-tauri cargo check clean, clippy only pre-existing style lints.
- 02:36 EDT — Council phase started. S1 (backend wiring) review panel round 1 spawned (security, correctness, feasibility, clarity).
- 02:44 EDT — S1 council verdict: DO NOT SHIP (confidence medium). Blocking: dead backends report "running" forever (reap_exited never called); dashboard throws TypeError on live path (BackendInfo/BackendRow contract break); telemetry wrapper discards the real {samples} payload AND the telemetry Poller was never started in src-tauri. Council log written.
- 02:52 EDT — S1 Rust fixes done and verified: reap on list_backends path; spec pruning (Supervisor::unregister_spec + spec accessor, with crate tests); non-config live backends surfaced in list_backends; start/restart preflight (enabled gate, mid-download gate, binary/model existence with remediation messages); list_models downloading flag; download destination aligned to gguf_path; raw_flags --host filtered; Windows drain window 3s in Supervisor::stop; telemetry Poller started on first use (NVML -> nvidia-smi fallback, cadence from config) with peaks merged into get_gpu_telemetry; src-tauri unit tests for backend_spec/process_state_str (type-checked; cannot link on this VM — no GTK libs). Gates: 314 workspace tests pass, fmt/clippy clean, src-tauri check + check --tests clean.
- 02:56 EDT — TS fix batch delegated (api.ts telemetry + backends mapping, dashboard null-safety, types honesty, Models downloading state, Backends configured/enabled states, smoke guard for withGlobalTauri).
- 03:04 EDT — S1 re-review verdict: SHIP WITH FIXES (Judge, high confidence). All three original blockers verified closed. Fix batch #2 applied and verified (314 workspace tests, clippy/fmt clean both trees, UI build + smoke pass): terminal download handles reaped on all read paths (is_terminal added to manager-models); download_model check-and-insert is atomic under one lock hold and refuses re-download of a model loaded by a running backend; verify/delete resolve gguf_path under data_dir (delete_model_file takes an explicit file path); telemetry_set_interval persists and the GPUs view states it applies on restart; dashboard gauge ring draws a neutral track when the metric is absent; prune/surface scoped to backend-owned spec ids (MCP specs untouched); start_backend reaps before spawn; preflight compares data-dir-resolved paths; unregister_spec clears per-id runtime history; stop() marks the id failed on DrainTimeout. Accepted with disclosure: no crash auto-restart (manual recovery by design), NVML source stubbed (nvidia-smi ships). One flaky wizard test observed under full-workspace parallel load (start_gateway_fails_with_install_guidance_without_uv); passes standalone, unrelated crate.
- 03:07–03:09 EDT — S2 verdict: SHIP WITH FIXES (Judge, high). S3 verdict: DO NOT SHIP (Judge, high) — SHA-256 never verified on the Models download path; download outcomes never reach the view; partial files present as complete; fresh installs show an empty Models view (no catalog seeding); model_delete doesn't persist. Both logged in council log. S2+S3 fix batch (Rust + TS agents) dispatched with a frozen contract (.part downloads, completion watcher with verification, catalog seeding, teardown pattern for all wireX functions, Backends polling + enable/test wiring, Models state machine).
- 03:12–03:23 EDT — S4 verdict: SHIP WITH FIXES (GPUs view never refreshes; placement unreachable from UI; per-process VRAM exposed nowhere; placement_apply doesn't persist; poll-interval control misrepresents cadence; telemetry failure indistinguishable from no GPUs). S5 verdict: SHIP WITH FIXES (logs "all" scope silently lossy and unchronological; restart destroys the failed backend's ring; no live tail; diagnostics bundle unwired). S6 verdict: SHIP WITH FIXES (LiteLLM spawned without --host — binds 0.0.0.0 unauthenticated; gateway_status probes nothing; OpenRouter key injection documented but not implemented; "Request log" tile mislabels a capacity as a count). S7 verdict: SHIP WITH FIXES (mxc_set_mode never persists; update_mxc_policy writes validator-rejected policies; "Active policy" card renders the envelope, not the rules; mode buttons undefined, self-test unwired). S8 verdict: SHIP WITH FIXES (gateway state asserted not measured on map/sidebar/dashboard; contradictory "All systems normal" rules; map paints once under a "live data" label; placement card empty; mini-map hardcoded nodes). All verdicts logged in council_log_2026-10-15.md before acting.
- 03:18 EDT — S2/S3 fix batch landed in the tree (both agents; files written). TS agent report received (build + smoke pass, 39 checks). The Rust agent's completion report never arrived; its code is in the tree.
- 07:05 EDT — Orchestrator verification of the merged tree: 317 workspace tests pass, 0 fail; fmt clean; clippy clean except 3 trivial style warnings in new manager-models test code; src-tauri cargo check + check --tests clean; UI build passes; smoke all checks pass. Remaining: S2/S3 re-review council, then fix batches for S4, S5, S6, S7, S8 (in that order), each with a re-review, then release v0.1.2. Outstanding gap found by S8 seats: wireWizard also attaches a root click listener with no teardown (not covered by the S2/S3 TS batch) — fold into the S5 TS batch.

## 2026-10-15 07:20 EDT — wave-1 (S4/S5 fixes) landed; S2/S3 re-reviews logged

- S2 re-review verdict logged: SHIP WITH FIXES (Judge, high). One blocking item: backend_apply_config does not persist the enabled flag (in-memory only, persisted:false, view ignores it). Queued for wave-2 Rust. Rest non-blocking lows, mostly queued for wave 2.
- S3 re-review verdict logged: SHIP WITH FIXES (Judge, high). DO NOT SHIP lifted; all 5 original blockers verified closed. Three blocking mediums queued for wave 2: (1) verification not ordered before use (post-rename window; failed mismatch-delete ignored) — gate start until the watcher resolves + durable verified_at gate; (2) delete-all undone by catalog re-seeding — once-only seeding marker; (3) verified_at badge staleness — clear at download start and in the mismatch branch, gate badge on on_disk.
- Wave-1 Rust (S4/S5): ProcessSample gpu_index from the enclosing XML gpu element (minor_number order); telemetry store records last poll error + last success time; get_gpu_telemetry returns {samples, processes, at_ms, source_error} with PID→backend matching; nvidia-smi resolved to C:\Windows\System32\nvidia-smi.exe on Windows when present; placement_apply at parity with propose (split-length check, persists, finish_task) and model_assign finish_task; placement_propose gains snapshot_age_ms/gpu_indices/weights_only; LogRecord gains source + archived; logs_query "all" merges all rings chronologically with total_matching; one archived ring generation survives respawn; log_scopes(); spawn_reader caps lines at 64 KiB with a truncation marker; get_logs returns {lines, total_matching, truncated, scopes}.
- Wave-1 TS (S4/S5): GPUs view polls every 2 s (teardown-aware), source + data-age header, source_error state distinct from empty list, per-GPU stale chip, interval select reduced to Off/1s/5s initialized from config with effective-cadence wording, per-GPU process tables, fan/mem-util rows, GiB labels, Placement section (propose → weights-only + telemetry-age labels → apply with persisted reporting); dashboard placement derived from config tensor splits with an explicit empty state; Logs view: hash-param scope (#/logs?scope=<id>), Follow poll, level + search controls, archived scopes labeled, truncation line, session-retention note, full-date title attributes, source chips, split empty states, Export diagnostics button with the unredacted-logs warning; wireWizard teardown; charts.ts esc() in stackedBar/gaugeRing.
- Orchestrator gates on the merged tree: 323 workspace tests pass, 0 fail; fmt clean; src-tauri cargo check + check --tests clean; UI build clean; smoke all checks pass (42 ok lines).
- Next: S4/S5 re-review panels; then wave-2 fix batch (S6 gateway, S7 settings, S8 map/dashboard, plus the S2/S3 re-review leftovers above); then S6/S7/S8 re-reviews; then release v0.1.2 under the standing default-YES.

## 2026-10-15 07:27 EDT — S4/S5 re-reviews logged; wave-2 fix batch dispatched

- S4 re-review verdict logged: SHIP WITH FIXES (Judge, high). Mechanism layer verified closed by all seats. Three blocking items for wave 2: (1) [high] placement not completable end to end — default-config backends match no model in the Placement select, and placement_apply dead-ends on assigned_backends, which no production path populates; (2) placement_propose is catalog-only while the select offers config-only models; (3) get_gpu_telemetry at_ms stamped at serve time, so the header always reads "updated just now" even during a source outage.
- S5 re-review verdict logged: SHIP WITH FIXES (Judge, high). Both original blockers verified closed with re-run tests. Two blocking disclosure items for wave 2: the one-generation archive limit is stated nowhere; the picker label becomes "(incl. previous run)" with labels refreshed on merge.
- Wave-2 Rust batch dispatched: S6 (LiteLLM --host 127.0.0.1, OpenRouter credential injection via CredReadW, gateway_status live probe + real routing_log_len, trace_routing dry-run honesty, update_gateway_config persistence, YAML/doc reconciliation + group-name sanitization), S7 (mxc_set_mode persistence, update_mxc_policy validator gate, update_config install-on-success, diagnostics version from CARGO_PKG_VERSION, config_path in get_config, bundle via log_scopes with archived markers), S2 closure (backend_apply_config persistence, ApiError Display mapping, orphan model_file normalization), S3 closure (verifying set + durable verified_at preflight gate + verification-failure outcomes gating start, attempt tokens, catalog_seeded marker, manager-models 206-mismatch/stale-416 tests + clippy fixes), S4 closure (placement_apply assignment fallback + clone/install pattern, propose resolves config models + free_mib in projection rows, at_ms anchored to last successful poll + has_polled, telemetry store prunes vanished GPUs, wizard nvidia-smi hardening, doc corrections).
- Wave-2 TS batch dispatched: S6 Gateway view (probe-keyed status + poll, routing-trace tile, endpoint/auth card, OpenRouter tier card, routing honesty, YAML preview corrections, Start/Restart via the wizard step), S7 Settings (real policy rules, mode definitions + Disabled confirm, self-test button, error-vs-absence, per-component updates, diagnostics export, config path row), S8 (sidebar/dashboard gateway from the probe, one shared verdict rule, Service Map polling + tensor-split edges + config-derived labels, mini-map de-fabrication, dead series removal, poll disposed guard + stale-while-paused), plus S2/S3/S4/S5 re-review leftovers (badge gating, spawn-window buttons, per-card in-flight disabling, logs label refresh + generation disclosure, GPUs degraded dot + peak label, and the rest).
- Next: gates on the merged wave-2 tree; S6/S7/S8 re-review panels; then release v0.1.2 under the standing default-YES.

## 2026-10-15 ~08:03 EDT — S6/S7/S8 re-reviews complete; wave 3 dispatched

All three re-review judges ruled SHIP WITH FIXES (high): S6 (blocking: spawn_detached env drop [high]; no gateway stop / restart-while-up no-op [medium]), S7 (blocking: updates_pin/updates_set_auto persistence [medium]; setup_mark_complete ok-on-failure [low]), S8 (blocking: placement UI gate dead-ends on app-produced configs [high]). All original blockers in S6/S7 closed; S8 has 6 of 7 closed. Wave 3 (Rust + TS in parallel) covers every blocking item plus the cheap convergent lows (sidebar poll, POLICY-CHECKED-when-unknown, verdict gateway-down suffix, probe any-HTTP-status, mxc path alignment, atomic policy write, gateway credential-presence display, mini-map state honesty, per-node map links). After wave 3: targeted verification seats for the three blocking fixes, full gates, then the v0.1.2 release under the departure default-YES.

## 2026-10-15 ~08:20 EDT — Wave 3 verified; council closed; releasing v0.1.2

Wave 3 landed (345 workspace tests, all gates green on the merged tree) and all three blocking fixes were verified by targeted seats: gateway env delivery + stop/restart with pid-safety guards, Settings persistence conversions (full-file sweep clean), placement end-to-end on the default config. All 8 sections: reviewed → fixed → re-reviewed SHIP WITH FIXES → blocking items fixed and verified. Proceeding to the v0.1.2 release (departure default-YES): version bump, push, publish, watchdog on the Windows build (first real compile of the CredReadW block).

## 2026-10-15 ~08:48 EDT — v0.1.2 RELEASED AND VERIFIED; AFK session complete

Release run 38734172514 success, CI run 38734172460 success, Install Test run 38736912665 success (clean-runner download, silent install, launch, silent uninstall). Asset: Local.LLM.Service.Manager_0.1.2_x64-setup.exe, 4,245,400 bytes. The Windows build was the first real compile of the cfg(windows) CredReadW block and the tasklist/taskkill gateway code — both compiled. Gap found and fixed during release verification: install-test.yml had never reached GitHub (gh_sync skips .github), so no install test fired for v0.1.1/v0.1.2; the file was pushed via the git-database API (commit 00050c36) and the v0.1.2 test dispatched manually. Watchdog cron llm-release-v012-watch disabled itself after the final report. Remaining validation: Pete's Precision hardware (wizard, models, GPUs) — not provable from CI.

## Oct 15, 2026 — v0.1.3 and v0.1.4 hotfixes (post-AFK, hardware feedback loop)

- v0.1.3 (commit c830266e, asset 4,247,665 bytes): fixed the two defects from Pete's first hardware run of v0.1.2 — wizard step 2 gained the verified built-in llama.cpp pin (b11658 cuda-12.4 + cudart, hashes from the official downloads) and CREATE_NO_WINDOW was applied at all 8 Windows spawn sites (the telemetry nvidia-smi poll had flashed a console window every 2 s). Release + CI + install test (dispatched run 38765680302) green.
- v0.1.4 (commit 229b5676, asset 4,258,161 bytes): fixed the defects from Pete's v0.1.3 run — gateway venv pinned to Python 3.12 (uv's unpinned pick of 3.15 broke the litellm[proxy] install: hiredis has no cp315 wheel and the source build needs MSVC), venv version/import probes with reuse of a healthy venv and --clear recreation of a wrong-version one, wizard Running state shown in the UI with all controls disabled while busy, backend try_lock rejecting duplicate run commands, and audit results painted into the checklist. 353 workspace tests, clippy clean, src-tauri check clean. Release run 38768520660 + CI 38768521163 + install test 38769914834 all success.
- Next checkpoint remains Pete's: install v0.1.4, run start-gateway (his 3.15 venv self-heals), confirm the wizard feedback states.

## Oct 15, 2026 — v0.1.5 hotfix (gateway spawn + log capture + wizard hydration)

- v0.1.5 (commit 06b61a6f, asset 4,279,969 bytes): Pete's manual PowerShell run proved the gateway had never started in any version — the wizard spawned `python -m litellm`, but LiteLLM ships no `__main__`; the process died instantly and the detached spawn discarded the output, so the only symptom was a 30-probe health timeout. Fixes: spawn via the venv's litellm console script; gateway stdout/stderr captured to logs/gateway.stdout|stderr.log (rotated per run, one previous kept), tails surfaced in the step failure and merged into the Logs view as a "gateway" scope; health wait checks process liveness and fails fast, budget raised to 120 s; wizard checklist hydrates from audit checks at launch and setup_get_steps serves a cached snapshot while a step holds the lock. 357 workspace tests, clippy clean, src-tauri check clean. Release run 38790158872 + CI 38790158949 + install test 38791457148 all success.
- Next checkpoint remains Pete's: install v0.1.5 and run start-gateway from the wizard; if it fails, the step outcome now contains the gateway's own log tail.

## Oct 15, 2026 — v0.1.6 hotfix (gateway master key + prisma)

- v0.1.6 (commit 81bc32df, asset 4,284,881 bytes): Pete's manual litellm.exe run booted LiteLLM but it refused to start without a master key (UnsafeMasterKeyError — LiteLLM 1.105 enforcement), and once a key was set manually, an unauthenticated GET /v1/models returned 500 because the proxy's auth exception handler imports prisma, which is not part of litellm[proxy] (verified in PyPI metadata; prisma sits in the extra-proxy extra). Fixes: generated config declares general_settings.master_key as os.environ/LITELLM_MASTER_KEY; ensure_gateway_master_key generates the key on first use and stores it in Windows Credential Manager (CredWriteW), injected at spawn; health, audit, and smoke-test probes send it as a bearer (new http_get/post_with_headers ops); the Continue editor config (wizard step and vscode_write_config command) carries it as apiKey; pip install spec adds prisma and the venv probe imports it, so Pete's existing venv self-heals on the next start-gateway run. 357 workspace tests, clippy clean, src-tauri check clean. Release run 38795969465 + CI 38795969558 + install test 38797332864 all success.
- Next checkpoint remains Pete's: install v0.1.6, close the manual gateway instance, run start-gateway from the wizard end to end, then the smoke test.

## Oct 15, 2026 — v0.1.7 (gateway UTF-8 stdio, telemetry power N/A, uninstaller, diagnostics)

- v0.1.7 (commit fa4ce366, asset Local.LLM.Service.Manager_0.1.7_x64-setup.exe, 4,285,012 bytes): four defects from Pete's v0.1.6 hardware runs. (1) Wizard start-gateway crashed on LiteLLM's startup banner: with stdout redirected to the captured log file, Windows Python encodes via cp1252, which cannot represent the banner's box-drawing characters (UnicodeEncodeError, "Application startup failed"). The spawn env now forces PYTHONIOENCODING=utf-8 and PYTHONUTF8=1. (2) Telemetry source failed entirely because the RTX A4000 reports power_draw N/A and smi.rs required it; power_w is now Option<f32> end to end (peaks skip it, views render N/A). (3) The uninstaller's user-data removal used NSIS RMDir /r over the ~15k-file Python venv (minutes, per-file logging); it is now one hidden rd /s /q via nsExec with one log line, RMDir kept as fallback. (4) Diagnostics bundles omitted the file-backed gateway logs (Pete's exported bundle had none); the bundle now includes the gateway scope, and both export sites show an Open folder button (new open_folder command). 358 workspace tests, clippy clean, src-tauri check clean. Release run 38800373205 + CI 38800373260 + dispatched install test 38801793209 all success.

## Oct 15, 2026 — v0.1.8 (models registry, wizard reporting, vscode skip, manual commands)

- v0.1.8 (commit a0b200f7): Pete's next round of hardware findings. (1) The wizard download step reported "no models configured" on a fresh install: catalog seeding lived only inside the Models commands, so the wizard snapshot could be empty. Seeding now runs in wizard_ctx construction; seeded models start enabled and pre-assigned to their default backends, and untouched default backend model_file paths are aligned to the seeded model paths. (2) All four catalog SHA-256 values were placeholders that fail closed; they are now the real pins from the publishers' Hugging Face LFS metadata (Qwen3-32B efd97156…, Qwen3-Coder-30B fadc3e5f…, Qwen2.5-Coder-7B 509287f7…, Qwen3-8B d98cdcbd…) with real sizes. The coder entry's URL was dead (Qwen publishes no Qwen3-Coder-30B-GGUF repo; 401) and now points at unsloth's GGUF conversion. (3) Models had no enable/disable: ModelConfig.enabled added (absent = enabled); the wizard downloads enabled models only; models without a pinned hash count as present-but-unverified instead of failing an impossible comparison. New model_add / model_set_enabled commands; Models view gains per-card enable/disable and an add-model form. (4) A concurrent audit or step invocation returned "wizard error: an audit is already running"; it now returns an already-running status that the UI paints as a status line (the usual trigger is the launch-time hydration audit). (5) vscode-extensions hard-failed when the code CLI was absent (Pete's Precision has no VS Code); the step probes `code --version` and skips in both install and audit modes. (6) Failed wizard steps render the exact commands they run as a PowerShell block with a copy button (absolute paths, env as $env: lines, download/http pseudo-commands translated to curl.exe/Invoke-RestMethod, secrets never printed; the master key renders as a self-generated value for manual runs). 362 workspace tests, clippy clean, src-tauri check clean, UI build clean. Release run 38805408447, CI 38805408425, watchdog llm-release-v018-watch armed.
- Next checkpoint remains Pete's: install v0.1.8, run the wizard end to end (models download ~45 GB across the four defaults), enable + start the backends from the Backends view, then the smoke test. Open item: Pete has not yet said whether the wizard should start backends itself before the smoke test.

## Oct 15, 2026 — v0.1.9 (config heal, fleet enabled by default, wizard starts backends, model dropdown, placement GPU selection)

- v0.1.9 (commit ec2a3731): Pete's 16:54 round on v0.1.7. (1) Blocking: every backend enable failed validation — his config was seeded pre-v0.1.8 with PLACEHOLDER hashes and params_b 0.0, and catalog_seeded=true meant v0.1.8's better seeding never re-ran. A one-time repair (manager_config::repair_catalog_defaults, gated on the new defaults_repaired flag so it runs exactly once and never overrides later operator edits) replaces placeholder hashes with the real catalog pins, fills params_b (catalog now carries 32/30/7/8), records default backend assignments, aligns untouched-default backend model_files, and enables the default fleet. Deliberately emptied hashes and custom model_files are left alone. (2) Fresh installs: the seeding pass now enables the four backends after alignment (raw default_config keeps them disabled because enabled+unregistered fails validation before seeding; update_config now seeds first so a Settings save on a fresh install cannot hit the pre-seed config). (3) The wizard now starts enabled backends before the smoke test (UI-side, both the Run setup loop and the per-step path; wizard steps have no supervisor handle), and the smoke test waits up to 240 s per enabled member backend for /health 200 in Install mode before probing (audit unchanged). (4) mxc-policies skips when wxc-exec.exe is absent (same spawn-failed pattern as vscode-extensions) — the MXC runtime is Microsoft's @microsoft/mxc-sdk distribution, not bundled; whether the wizard should install it (pinned binaries + host prep) is Pete's open decision. (5) Backends view: per-backend model dropdown wired to model_assign carrying the current tensor split. (6) GPUs placement: per-GPU include checkboxes; excluded GPUs get a 0 fraction and a "not used" fit row; explanation line added. WinML default unchanged (disabled; nothing routes to it, enabling requires an external WinMLServer) — recommended to Pete as-is. 366 workspace tests, clippy clean, src-tauri check + check --tests clean, UI build clean. Release run 38808239050, CI 38808239159, watchdog llm-release-v019-watch armed. v0.1.8's pipeline finished fully green in the meantime (release 38805408447, CI 38805408425, dispatched install test 38806458148, installer 4,308,765 bytes).
- Next checkpoint is Pete's: install v0.1.9 — the existing config heals on first launch (no deletion needed), the two already-downloaded models can be verified against the real pins — then run the wizard end to end incl. smoke test.

## Oct 15, 2026 — v0.1.10 (installer Install/Upgrade options, wizard running display)

- v0.1.10 (commit 4ebdc8df): (1) Installer upgrade path (Pete's request): the stock tauri-bundler NSIS template already had a reinstall page for existing installs, but on upgrade it pre-selected "Uninstall before installing" and recommended uninstalling in its body text. The repo now vendors the template (src-tauri/installer-template.nsi, based on the tauri v2.12.2 stock template matching the locked tauri 2.12.2; wired via bundle.windows.nsis.template) with two marked patches: on upgrade the options read "Install (remove the existing version first)" and "Upgrade (install in place)" with a neutral body text (explains the user-data prompt on the Install path), and Upgrade is pre-selected whenever a previous version is being upgraded (an explicit back-navigation choice is honored). No previous install = the page is skipped, plain install (Install is the default by construction). Same-version maintenance (Add/Reinstall vs Uninstall) and downgrade (uninstall-first default) are stock, deliberately. Install's removal path runs the old uninstaller, whose existing hook asks before removing user data (default: keep); silent installs keep the stock in-place behavior and never prompt. Re-base the vendored template when the tauri version changes (header comment in the file). (2) Wizard running display (Pete's request): a step that is running now renders blank and blue (state "running", accent border + tint, detail/message/log/manual blocks cleared) regardless of its previous state — previously the running override only applied to queued steps, so a retried failure stayed red with its old message for the whole re-run. During a full Run setup or an audit, steps without a result in the current run also render blank (queued, neutral) instead of the previous session's success/failure; results paint in as each step completes. 366 workspace tests, clippy clean, src-tauri check clean, UI build clean. Release run 38813906759, CI 38813906848, watchdog llm-release-v0110-watch armed (makensis compilation of the vendored template is the release's known first-time risk).
- Next checkpoint is Pete's: install v0.1.10 over v0.1.9 — the installer should present Upgrade pre-selected — then the wizard end-to-end run.

## Oct 15, 2026 — v0.1.10 verified; v0.1.11 (gateway window, wizard persistence, VS Code detection, smoke probe)

- v0.1.10 pipeline fully green: release 38813906759, CI 38813906848, dispatched install test 38815261774, asset 4,319,635 bytes. The vendored NSIS template compiled on the first attempt.
- Pete's v0.1.9 diagnostics bundle (diagnostics-1792102628834.zip) confirms the config heal end to end on his hardware: defaults_repaired=true, all four models carry the real pins AND verified_at timestamps (all four downloaded and checksum-verified, including the 32B planner), all four backends enabled with aligned model files. It also pinned down three more defects, all fixed in v0.1.11:
  (1) Gateway console window: spawn_detached spawned the venv's litellm.exe, which is a uv/pip trampoline that spawns python.exe as its own child without forwarding CREATE_NO_WINDOW; having no console itself, Windows gave python.exe a fresh console window for the gateway's lifetime (and the pidfile held the trampoline's pid, not the server's). Fix: spawn the venv interpreter directly (pythonw.exe when the venv provides it, else python.exe) with -c "import sys; from litellm import run_server; sys.exit(run_server())" — the console script's exact entry point, verified against the litellm 1.105.1 wheel's entry_points.txt (run_server is a click command parsing sys.argv[1:]).
  (2) VS Code step skipped on Pete's machine although VS Code is installed and `code` works in his PowerShell: the step probed bare `code` via the app process's PATH, which a GUI process inherits from its launcher and may predate the install. Fix: SystemOps::vscode_cli() — RealSystemOps resolves the CLI from the registry App Paths entry for Code.exe (HKLM, then HKCU) plus standard install locations (Program Files / LocalAppData Programs, stable + Insiders); the step uses the resolved absolute path for probe/install/list and keeps the bare-`code` PATH fallback; skip messages distinguish "not installed" from "found but could not start".
  (3) Smoke test false failure "empty completion" for planner: planner.log in the bundle shows the probe's 23-token prompt and exactly 8 generated tokens — the probe's max_tokens was 8, and Qwen3-32B spends its first tokens inside the think block, which llama.cpp reports as reasoning_content, leaving content empty. Fix: the probe sends chat_template_kwargs enable_thinking=false and max_tokens 128 (shared probe_body for run + planned commands), a reasoning-only response counts as a pass (it proves routing + generation), and the failure message now includes finish_reason. Latency bound unchanged (120 s; the planner probe took 13.8 s — prompt eval 1.92 tok/s, generation 3.90 tok/s on the split 32B).
  (4) Wizard progress persistence (Pete's request): the install wizard's report is saved to <data_dir>/wizard-state.json after every step run/retry/skip/mark-manual and after startup hydration; at launch it is restored before the hydration audit runs (Wizard::restore — adopts all outcomes incl. failures/skips, rejects other-mode reports, a step recorded Running returns Queued with an "interrupted — the app closed" note).
  (5) Diagnostics bundle telemetry was [] again: the telemetry poller starts lazily on first telemetry() call, so a wizard-only session exported an empty snapshot; the export now waits up to ~2.5 s for the first poll to land.
  370 workspace tests, clippy clean, src-tauri checks clean.
- Next checkpoint is Pete's: install v0.1.11 (Upgrade pre-selected over either prior version) and re-run the wizard: gateway should start with no console window, the VS Code step should find his install, smoke-test should pass for planner, and closing/reopening the app mid-setup should keep the checklist state.
- v0.1.11 pipeline verified fully green by the watchdog (Oct 15, 2026, 18:36 EDT): Release run 38816951924 success, CI run 38816951830 success, dispatched Install Test run 38818066427 success; asset Local.LLM.Service.Manager_0.1.11_x64-setup.exe, 4,333,557 bytes.
- Pete, 18:29 EDT: the Nemotron evaluation had been delivered (18:08) but not acted on; he also reported (18:32) that the Service Map showed the A4000 holding 15.4 GB while every backend role denied holding anything (health/throughput/VRAM all "—").
  VRAM diagnosis: per-role VRAM attribution joins nvidia-smi per-process data to the supervisor's pid set from the CURRENT app session only. Backend llama-servers are detached and survive the app closing; after his restarts, the new session supervised nothing, so every role read "—" while the ownership-agnostic GPU totals showed the memory held. Open sub-question: on the WDDM display GPU, per-process used_memory may be N/A (the telemetry parser stores 0 for N/A) — Pete to run `nvidia-smi --query-compute-apps=pid,process_name,used_memory --format=csv` to settle it; if N/A, per-process VRAM needs the Windows PDH "GPU Process Memory" counters in a follow-up. Also flagged: one P100 in his paste sat at 81 °C at 0% util (passive card, known airflow constraint) and only one of two P100s appeared in the pasted view.
- v0.1.12 SHIPPED Oct 15, 2026 (main fc02f77edc7d1f7828ab8e382aaec752ca4a6689; tag v0.1.12; watchdog llm-release-v0112-watch armed). Gates: 373 workspace tests, clippy 0, fmt clean, src-tauri check + check --tests clean, UI build clean. Content:
  (1) Nemotron 3.5 Lightning 30B-A3B added to the curated catalog as an opt-in candidate: unsloth UD-Q4_K_M GGUF, 25,266,255,936 bytes, SHA-256 edcb5d4650796ed2fb412498de6f83b585862312c747ddb74f0ea04b22206181 (from Hugging Face LFS metadata), params_b 31.6, ModelSpec.enabled_by_default=false, no default backend — seeds disabled and unassigned; the wizard's download step filters on enabled, so it is never fetched unless Pete enables it in Models and assigns it (built for A/B against Qwen3-Coder-30B in the coder role). Delivery mechanism for existing installs: revision-gated catalog sync — manager-models CATALOG_REVISION=1, AppConfig.catalog_revision, manager_config::append_missing_catalog_models; ensure_catalog_models appends missing catalog models once per revision (a deliberately deleted model is not resurrected until the catalog next changes). Fresh seeding honors enabled_by_default.
  (2) Backend adoption: manager-supervisor gains SupervisedProcess.adopt (pid-tracked, no child handle; drain waits out the window then kills the tree via taskkill /T /F on Windows, SIGKILL on unix; kill via the same path; adopted deaths detected by pid liveness in reap_exited and marked Failed), ProcessStatus.adopted, and the Windows helpers port_owner_pid (GetExtendedTcpTable, TCP_TABLE_OWNER_PID_LISTENER) + process_image_is (QueryFullProcessImageNameW) + pid_alive. src-tauri adopt_orphan_backends runs at the head of list_backends and get_gpu_telemetry: for each configured backend not already supervised, if the port owner is a live llama-server.exe it is adopted — health, state, and pid-keyed VRAM attribution now survive app restarts. list_backends rows carry "adopted"; the Backends view shows an "adopted" chip and a note (pre-adoption output is not in this session's Logs). 2 new supervisor tests cover the adopted lifecycle and adopted-death reaping.
- Next checkpoint is Pete's: install v0.1.12 (Upgrade pre-selected) — v0.1.11 wizard checks carry over (no gateway window; VS Code step finds his install; smoke passes planner; wizard state persists across relaunch), plus: after a restart with backends left running, the Backends/Service Map views should show them running (adopted) with VRAM attributed instead of "—".

## v0.1.13 shipped (Oct 15, 2026)
- Main commit f9644627674f179524ea00ebde837811a13869b1; tag v0.1.13. Gates at ship: 379 workspace tests, clippy 0 warnings, fmt clean, src-tauri cargo check + check --tests clean, UI build clean. Watchdog llm-release-v0113-watch armed for release/CI/install-test verification.
- MXC install step (install-mxc; wizard is now 11 steps): pinned @microsoft/mxc-sdk 1.0.0 tarball (SHA-256 39e59ffd9d0832334889b0cd583d0beb43b1bbbe9f783dd7c6980203d23e9602, 37,746,375 B) downloaded by the step; package/bin/x64 extracted whole into <data_dir>/mxc/bin/x64 via new SystemOps::extract_tar_gz; wxc-exec.exe (8a6d1db4…) and wxc-host-prep.exe (c07c94ac…) verified against pins, fail closed on mismatch. Host prep runs elevated (new SystemOps::run_elevated, ShellExecuteExW runas) as one UAC prompt chaining prepare-system-drive + prepare-null-device from a generated host-prep.cmd; a declined/failed prep completes the step with the outstanding state in the message and marker host_prep=false so re-runs retry only the prep. Marker: <data_dir>/mxc/.installed.json. Audit verifies marker + binary hashes only (null-device prep resets every boot by design).
- MXC CLI contract fixes (found by the SDK research against the real 1.0.0 binary): probe --probe-json -> --probe; self-test --policy/--self-test -> --config <policy> --dry-run -- cmd.exe /c echo llm-manager-mxc-self-test; MxcPolicies resolves <data_dir>/mxc/bin/x64/wxc-exec.exe first with a bare-name PATH fallback (resolve_wxc_exec); planned commands name the canonical installed path.
- Gateway credentials UI (implemented locally Oct 15, shipped here): gateway_master_key command, master-key reveal/copy in the Gateway view, corrected client-endpoint copy (master key required; extension base URL without /v1), LiteLLM admin UI button with the database caveat stated.
- v0.1.12 verified fully green this session via API: Release 38819047507, CI 38819035321, dispatched Install Test 38820162615, asset 4,347,670 bytes.

## v0.1.13 verified green (Oct 15, 2026, via API)
Release 38824432145 success; CI 38824432166 + 38824395129 success; dispatched Install Test 38825335663 success; release published 2026-10-15T23:42:30Z; asset Local.LLM.Service.Manager_0.1.13_x64-setup.exe 4,382,763 bytes. Watchdog llm-release-v0113-watch self-disabled (its consolidated chat report did not surface in the side chat; outcome verified directly via the API instead).

## v0.1.14 shipped (Oct 15, 2026)
Commit 625d7c88367aeecbf2b9a47ed45e13caafda0b17; tag v0.1.14; watchdog llm-release-v0114-watch armed. Gates at ship: fmt clean, 386 workspace tests, clippy 0, src-tauri check + check --tests clean, UI build clean.
Content (VS Code step expansion + Continue fix):
- Extensions: vivswan.litellm-vscode-chat added to the install set (Continue, Cline already there).
- LiteLLM endpoint: settings.json entry (label "Local", baseUrl without /v1, master key inline auth) merged via jsonc-parser CST — user comments/formatting/other settings survive; own entry replaced by label or baseUrl match.
- Copilot agents: ~/.copilot/agents/litellm-<group>.md per gateway group; planner text verbatim from Pete; coder/coder-fast/tool-runner authored in the same structure (tools [read, search, edit, execute] / tool-runner [read, search, execute]).
- Continue: config.yaml + .env per Pete's design, verified against continuedev/continue 1.3.40 (research: ~/workspace/.tmp-assets/continue-litellm-config-research.md). apiBase WITH /v1; apiKey ${{ secrets.LITELLM_API_KEY }}; roles [chat, edit, apply]; capabilities [tool_use] (mandatory — autodetection would deny these model names); existing config.yaml merged by model+apiBase identity, .env merged by key; legacy config.json no longer written and a wizard-written one is moved to config.json.llm-manager.bak; vscode_write_config Tauri command migrated to the same pair with backup-first semantics.
- Audit verifies the settings entry, agent files, config.yaml models, and the .env key.

## v0.1.14 verified green (Oct 15, 2026, via API)
Release 38827831397 success; CI 38827831396 + 38827823193 success; Install Test 38828759239 success; published 2026-10-16T00:14:19Z; asset 4,543,282 bytes.

## v0.1.15 shipped (Oct 15, 2026)
Commit f58ab4bebd9b3748cbcfbfe97753adf6e59c6c1d; tag v0.1.15; watchdog llm-release-v0115-watch armed. Gates at ship: fmt clean, 386 workspace tests, clippy 0, src-tauri check + check --tests clean, UI build clean.
Content (requested by Pete Oct 15 — no way existed to give the Nemotron catalog model a backend):
- backend_add / backend_delete Tauri commands (clone-validate-save; seeded defaults n_ctx 32768 / n_batch 512 / restart 5x2s; group join-or-create; delete refused while running and scrubs group memberships + model assignment lists).
- Backends page: "Add backend" form (name, model dropdown, next-free port prefilled, existing-or-new group, enabled default) in a poll-safe container outside the repainted grid; post-create card offers an inline gateway restart (routes generate at gateway start). Remove button on stopped/failed configured cards with a two-click confirm.
- PostgreSQL moves to v0.1.16 (research still running).

## v0.1.15 verified green (Oct 15, 2026, via API)
Release 38829095077 success; CI 38829095101 + 38829087714 success; Install Test 38830312722 success; published 2026-10-16T00:30:09Z; asset 4,561,723 bytes. Watchdog llm-release-v0115-watch self-disabled.

## v0.1.16 shipped (Oct 15, 2026)
Commit 914c05246ca8c5710b2fc489375def3f51fcee1f; tag v0.1.16; watchdog llm-release-v0116-watch armed. Gates at ship: fmt clean, 390 workspace tests, clippy 0, src-tauri check + check --tests clean, UI build clean.
Content (app-managed PostgreSQL, Pete's decision Oct 15 — the LiteLLM admin UI cannot be logged into without a database):
- Research landed first (~/workspace/.tmp-assets/postgres-litellm-research.md): pin PostgreSQL 18.6-1 EDB binaries zip (343,808,005 B, SHA-256 fbe23da2…, self-computed — EDB publishes none); LiteLLM auto-migrates at startup via proxy-extras prisma migrate deploy (verified empirically: 195 migrations / 91 tables on PG 17.11 and 18.6).
- manager-config: PostgresConfig { port } (default 54329, serde-default on AppConfig, claimed in validate).
- New SystemOps::extract_zip_prefixes (Real manual-entry impl with traversal rejection; Fake records + materializes) — the step extracts only pgsql/bin|lib|share|include (155.6 MiB of 871 MiB).
- install-postgres step (wizard now 12 steps, before start-gateway; prereq detect-hardware; audit = marker + binaries + PG_VERSION): download/verify zip fail-closed, extract, ensure password (Credential Manager local-llm-service-manager/postgres-password; generated hex = URL-safe), initdb (-U postgres -A scram-sha-256 --pwfile, pwfile always deleted), pg_ctl start (-o "-p 54329 -h 127.0.0.1", log to <data_dir>/logs/postgres.log), createdb litellm (already-exists tolerated), marker .installed.json.
- ensure_postgres_running shared helper (pg_isready poll; failure returns the server log tail — the admin-token refusal surfaces there); stop_postgres_server (pg_ctl stop -m fast).
- StartGateway: when installed, start PostgreSQL before spawn, pre-warm Prisma toolchain (python -m prisma --version, non-fatal warning), pass DATABASE_URL in the spawn env; not installed → spawns exactly as before. gateway_stop stops PostgreSQL after the gateway (postgres_stopped in payload). Gateway view admin-UI copy updated (run install-postgres if sign-in reports "Not connected to DB").
- 4 new wizard tests (install happy path incl. initdb/createdb argv + pwfile cleanup, checksum fail-closed, audit, gateway DATABASE_URL present/absent).

## v0.1.17 shipped (Oct 15, 2026) — gateway LAN bind
- Commit bc0ef65d on main; tag v0.1.17. Gates: fmt clean, 396 workspace tests, clippy 0 warnings, src-tauri cargo check + check --tests clean, UI build clean. Watchdog llm-release-v0117-watch armed for pipeline verification.
- GatewayConfig.bind_host (default "0.0.0.0", serde default for older configs; validate requires an IP literal). StartGateway spawns --host <bind_host>; all app-side probes use gateway_probe_host (wildcards -> 127.0.0.1): health wait, smoke test, main.rs gateway_status probe.
- Windows firewall reconciliation in the start-gateway step: rule "Local LLM Gateway (TCP <port>)", Private profile, localsubnet scope; unprivileged netsh show-rule check first; on mismatch, one UAC elevation runs a generated gateway/firewall.cmd (add = delete-then-add; remove = delete). Declined/failed elevation warns on the Done message, never fails the step. Loopback bind removes the rule.
- gateway_set_bind_host command (validate + persist; restart_required true). gateway_status payload gains bind_host + lan_ip (UDP-connect egress selection). Gateway view gains a Network access select in the Client endpoint card + the LAN base URL line when bound beyond loopback.
- 6 new tests (config default/invalid/legacy-deserialize, firewall add/remove/no-op, probe-host mapping); planned-command counts updated 5 -> 7 for start-gateway Install.

## v0.1.18 shipped (Oct 15, 2026) — Dashboard stats from LiteLLM Prometheus metrics
- Commit 6b07adfb on main; tag v0.1.18. Gates: fmt clean, 396 workspace tests, clippy 0 warnings, src-tauri cargo check + check --tests clean, UI build clean. Watchdog llm-release-v0118-watch armed.
- Generated gateway YAML gains litellm_settings.success_callback: ["prometheus"] (snapshot updated). Gateway pip spec gains prometheus-client (only in the proxy-runtime extra; the callback without it fails gateway startup — verified Oct 15); the venv probe now imports litellm, prisma, prometheus_client so older venvs fail the probe and receive the package on the next wizard run.
- New src-tauri module gateway_stats: Prometheus exposition parser (deployment success/failure counters by api_base, input/output token counters by model label; _total suffix and *_created series handled) + snapshot store (2 h retention) computing trailing-hour deltas per backend with counter-reset absorption. Unit tests compile-checked (src-tauri test binaries cannot link on this VM — known limit).
- Background poller in main.rs: every 15 s, reads the stored master key (never generates), GET /metrics via the probe host, records a snapshot; skips silently while the gateway is down or metrics are not yet enabled.
- list_backends rows gain requests_1h / errors_1h / throughput_toks when a window exists; api.ts carries them into BackendInfo and sums them onto the gateway card. Fields absent until two polls land (~30 s after the gateway restarts on the new config).

## v0.1.19 (2026-10-15)
- Placement fix: the wizard's configure-tensor-split step computed a split into wizard/tensor-split.json, a file nothing consumed, so every backend ran unsplit on the main GPU (diagnosed on Pete's Precision: the P100 held only 4x 256 MiB CUDA contexts, no weights). The step now assigns per-backend splits into the persisted config via manager_wizard::steps::assign_splits (valid existing splits kept and accounted; fitting models placed whole on the GPU with the most placeable room at 90% of VRAM; oversized models spread over remaining room; single-GPU machines left unset). Audit validates the config's splits. Backend starts emit the split via the existing backend_spec path.
- AppState::reload_config_from_disk after run_wizard_step and run_audit: step writes to config.json now reach the live in-memory config in the same session.
- backend_add assigns a split to a new backend from live telemetry (existing backends accounted, new one assigned); skipped silently without telemetry.
- Honest attribution: GPUs-view process rows under 1 GiB for a backend are labeled "context only" (a CUDA context is not model weights).
- WinML deferred (Pete, 2026-10-15): the full wizard setup is dropped until WinMLServer leaves Microsoft's experimental NuGet line; documented in docs/winml-future.md with the research at docs/winml-server-research-2026-10-15.md. The probe-only winml-register step stays inert.
- SystemOps::file_len added (Real + Fake) for on-disk model sizes.

## v0.1.20 (2026-10-15)
- Setup view fix: "Run setup" no longer blanks previously completed steps to "queued" for the duration of a run. paintWizard's hideStale mode rendered every step without a fresh result in the current run as blank/queued, and run-all skips done/manual/skipped steps entirely, so they never became fresh. The run-all handler now seeds freshIds with done/manual/skipped step ids after refresh(). Stored wizard state was never affected — rendering only.
- Pipeline verified fully green (checked directly via the runs API; the watchdog self-disabled): CI main 38844370832, CI 38844377218, Release 38844377268, Install Test 38845101891 — all success.

## v0.1.21 (2026-10-15)
- Restart port race fixed (Pete's report: "port in use" on backend restarts). For adopted processes, kill_pid_tree is fire-and-forget — stop() returned before the process exited, and spawn_backend probed the port exactly once, returning PortInUse while the dying process still held its listener. Spawned children were unaffected (their kill awaits wait()). This also explains the missing coder-fast startup logs: Pete's backends were adopted, so the restart failed at spawn and never produced a new process. Fix in manager-supervisor: wait_port_free(port, 10 s) polls before PortInUse is returned, and wait_pid_gone(pid, 5 s) confirms adopted processes have exited in both the drain and kill paths. New test wait_port_free_waits_for_release_and_times_out_while_held; supervisor suite 33 tests.
- Backends view grouped by gateway group (Pete's request): one section per group with a header (group name + member count), ungrouped rows and orphans last under "Ungrouped". list_backends rows carry the backend's group name.
- Backend flag editing (Pete's request — context length was not changeable after creation): new backend_update_flags command (n_ctx 1024..=1048576, n_batch 1..=n_ctx; validated, persisted, specs re-registered) and an Edit flags control on each configured backend card. Applies on next start. Cards also display current Context/Batch values; list_backends rows carry n_ctx/n_batch.
- Wizard output in the Logs view (Pete's request — install-postgres appeared stuck with no persistent logs): step runs append a start marker, the outcome header, and the step's log output to <data_dir>/logs/wizard.log (audit and hydration-audit runs included), served by get_logs as the "wizard" scope alongside the gateway's file-backed scope; rotation at ~1 MiB to wizard.prev.log. A hung step now leaves a timestamped "started, never finished" record.
- Download stall timeout: wizard downloads had a 30 s connect timeout but no stall timeout — a connection that stopped delivering bytes blocked the step (and the wizard lock) indefinitely. Each chunk read now has a 60 s no-data timeout that fails the download with a clear message; slow-but-progressing downloads are unaffected.
- Gates: fmt clean, 404 workspace tests, clippy 0 warnings, src-tauri cargo check + check --tests clean, UI build + smoke clean.

## v0.1.22 (2026-10-15)
- Gateway activation plans (Pete's report: adding a backend saved silently — no plan was created, nothing indicated a gateway restart was required, and dismissing the transient post-add card lost the information entirely). New src-tauri module `activation`: a persisted change journal (<data_dir>/activation-journal.json) records gateway-affecting saves — backend_add, backend_delete, backend_apply_config, update_gateway_config, gateway_set_bind_host — but only when the change alters the generated gateway YAML (bind host forced, since the YAML does not carry it), judged by comparing generate_config_yaml before/after. The first unresolved entry snapshots the pre-change config. Resolution rule: the start-gateway step regenerates gateway/config.yaml on every start, so entries older than that file's mtime are resolved; pending = unresolved entries OR live config drift (generated YAML vs the file). Commands: gateway_activation_plan, gateway_activation_cancel (restores the snapshot; refused when the config changed through another path since, with the reason), gateway_activation_complete, gateway_quiesce_wait (polls litellm_in_flight_requests from /metrics until zero or timeout). gateway_status gains activation_pending/activation_changes.
- UI: a persistent plan card at the top of the Backends and Gateway views — itemized changes, gateway state, and the operator options: "Quiesce, then restart" (drain in-flight requests first), "Restart gateway now", "Decide later" (collapses to a persistent one-line pending bar with Review), "Cancel changes" (two-click, restores the snapshot). When the gateway is down the plan offers Start gateway and states the changes apply at next start. The post-add card now points at the plan instead of offering its own transient restart. A manual gateway (re)start from the Gateway view also completes the plan.
- Scope note: literal traffic migration to a backup is not offered — LiteLLM fixes routes at process start and no dynamic route API is configured; quiesce drains in-flight work before the restart, and other group members keep serving until the restart itself.

## v0.1.23 — GPU residency, layered statistics, list ergonomics, number presentation

Pete's reports (Oct 15, 2026, late evening): the GPUs view process
table overflowed its cards; no view showed which backends are loaded
on which GPU; block lists were neither sortable nor filterable and
had no overview; statistics were missing at the model and gateway
layers; the Service Map did not show backends per GPU; dashboard
stat fields rendered as unexplained blanks; and raw floats
(15.1/15.9921875 GB on the console view) were presented unrounded.

- GPUs view: the process table is now a compact list inside a
  contained scroll region (max-height, overflow-y auto); each row is
  one line (backend or process name + VRAM + context-only chip) and
  clicks open to full detail (process, PID, per-GPU VRAM, backend,
  context explanation). Expansion state lives outside the repainted
  DOM so the 2 s poll cannot collapse an open row.
- Residency: a backend -> GPU map is built from telemetry's
  per-process attribution in the Backends view, the Service Map, and
  the dashboard. Backend cards show "Loaded on: GPU n (name) — X GiB"
  (context-only under 1 GiB; "not measured" when a running backend
  has no attributed process). Dashboard backend cards now fill VRAM
  HELD from the same data (previously no live source). Service Map
  edges prefer measured residency (configured splits only where
  nothing is measured), backend node subtitles carry per-GPU GiB,
  and a "GPU residency (measured)" card lists backends per GPU.
- Lists: Backends and GPUs views gain a short overview strip (one
  line per item) plus sort and filter controls (backends: name/port/
  state/requests + state filter + text filter; GPUs: index/util/
  VRAM/temp + name filter). Grouping by gateway group is preserved;
  sorting applies within sections.
- Statistics: backend cards render Requests (1h) / Errors (1h) /
  Throughput from the existing metrics window store (previously
  dashboard-only). Model cards aggregate the same figures over the
  backends serving the model. The Gateway summary card shows totals
  across backends plus the hottest backend. Where no metrics window
  exists, each surface says so in words — the dashboard shows an
  explicit "statistics are not available yet" card explaining the
  gateway-restart precondition instead of unexplained dashes.
  Per-deployment latency is not exposed by LiteLLM's verified
  Prometheus metric set and is not estimated.
- Number presentation: new crate manager-present holds the
  canonical rounding rules (trim1/gb/bytes/mib/count/percent/rate/
  latency_ms/duration_secs; unit tests). Wizard user-facing
  messages (GPU detection, split assignment, disk-space checks)
  format through it. The UI mirrors the rules in charts.ts
  (trim1/fmtGb) applied to every VRAM/GB figure, throughput rate,
  and step duration — the console view now reads 15.1/16 GB.

## v0.1.24 — install-postgres re-run over a live install (locked DLL)

Pete's report (Oct 16, 2026, 00:09 EDT): re-running the
install-postgres step failed with "I/O error for
...\postgres\bin\icudt77.dll: The process cannot access the file
because it is being used by another process (os error 32)". His
install was complete and the server running (the gateway uses it);
the step's skip-extraction guard required the .installed.json
marker, which was missing on his machine, so the step fell through
to re-extracting the distribution zip over the running server's
bin directory and Windows refused the overwrite of a loaded DLL.

Fix (manager-wizard steps.rs):
- The skip guard now keys on the ground truth: binaries present
  plus (marker OR initialized cluster). The marker is rewritten at
  the end of the run, restoring it.
- postgres_installed accepts the same evidence (binaries + marker
  or PG_VERSION) everywhere it is consulted — the audit no longer
  reports a working, marker-less install as "not installed", and
  start-gateway no longer withholds DATABASE_URL over a lost
  marker.
- When re-extraction genuinely is required, the step stops a
  running server first (pg_isready is the probe; pg_ctl stop -m
  fast) and restarts it through the existing
  ensure_postgres_running later in the same run; a failed stop
  returns a clear instruction instead of the DLL lock error.
- Regression tests: working install with the marker lost skips
  download + extraction and rewrites the marker; the extraction
  path issues pg_ctl stop before extracting. 412 workspace tests.

## v0.1.25 — headed overview lists, responsive reflow, topology residency accuracy
- Overview lists (Backends, GPUs, Models): rebuilt as a shared headed
  list — column headings, divider lines between rows, and a per-row
  status/health button (state-colored dot + icon) that scrolls to the
  item's card and flashes its outline. Models gained its overview list
  in this release (lifecycle status, serving backends, requests 1h).
  The Backends overview GPUs column names only GPUs with >= 1 GiB
  measured residency; context-only presence is labeled as such.
- Reflow: cards5/cards3 are fluid auto-fit grids; grid2 is two columns
  when wide and one column at <= 1100px viewport; model cards moved
  into a fluid grid (view width cap removed); the Service Map wraps
  under its inspector at <= 1250px; the wizard body stacks at <= 900px.
- Backend group sections are wrapped in a bordered group box.
- Service Map: backend->GPU edges no longer drawn from raw
  per-process attribution (a process holds a CUDA context on every
  visible GPU, which rendered every backend on every GPU). Solid
  edges require measured residency >= 1 GiB; live backends without
  measured residency get dotted configured-placement edges (tensor
  split, or the main GPU when no split is set); stopped/failed
  backends get no GPU edges; node subtitles show only real residency
  figures or "context only"; the legend states the rule.

## v0.1.26 — in-card flags editor with per-model preset options
- The Edit flags form now renders inside the backend's own card
  (it previously appeared above the list, where the click looked
  like a no-op). Draft values live outside the repainted DOM, so
  the 5 s repaint re-renders the open form without losing input;
  a rejected save keeps the form open with the error on the card.
- Context and batch are preset dropdowns, not typed fields:
  context ladder 8K..1M, batch 128..4096, each with a Custom
  numeric fallback. The catalog now rates each model's maximum
  context (ModelSpec.ctx_max, carried by list_models): presets
  beyond the backend's model rating are annotated "beyond rated"
  and the form states the model's rated maximum. Ratings: Qwen3-32B
  128K, Qwen3-Coder-30B 256K, Qwen2.5-Coder-7B 128K, Qwen3-8B 128K,
  Nemotron 256K (conservative). Custom models report no rating.

## v0.1.27 — lost GPUs show as down instead of vanishing
- Root cause of the "P100 missing" report: the P100 fell off the bus
  (`nvidia-smi -q -i 1`: "GPU is lost. Reboot the system to recover
  this GPU", PCI 51:00.0), so telemetry polls succeed with a
  one-GPU document — and the store pruned absent GPUs immediately,
  making the loss invisible in the UI.
- The store now retains a GPU that vanishes from polls, flagged
  down (GpuSample.down) with its last-known readings, peaks, and
  history; the first poll that reports it again restores it live.
  The GPUs view renders it with a red dot, a "down" chip, and a
  note that the readings are last known and that a lost GPU
  recovers only on reboot; the Service Map dot already had a down
  state and now receives it. Placement projection treats a down
  GPU as zero free VRAM and names it in the payload/UI, so no
  split is planned onto a dead card. Split-length validation
  still counts it (the split describes the machine's GPU set).

## v0.1.28 — Placement form is backend-only
- The GPUs Placement form now has one select: the backend. The
  model is derived from the backend's model_file (matched against
  the configured models) and shown as text; a backend whose file
  matches no configured model states that and cannot be proposed.
- placement_apply gained a backend scope: with a backend id, the
  split is written to that backend only (the command verifies the
  backend serves the model). The old form behavior is gone: Apply
  no longer reassigns the backend's model via model_assign, and no
  longer writes the split to every backend serving the model.
  The model-wide apply remains when no backend id is passed.

## v0.1.29 (Oct 16, 2026) — placement editing on the backend cards

Pete's direction: the tensor split is a per-backend setting, so it
is edited on the backend's card (the pattern the Edit flags form
established); the GPUs page only reports placement, never edits it.

- `list_backends` rows now carry `tensor_split` (the backend's
  configured fractions), so both the card form and the GPUs
  listing read placement from one payload.
- Backend card: a Placement button opens an in-card form — model
  derived from the backend's configuration, GPU include/exclude
  checkboxes from the live telemetry list, Propose renders the
  projection table inside the card (down GPUs treated as zero free
  VRAM, as on the old form), Apply writes the split to that
  backend only (v0.1.28's scoped `placement_apply`). Draft state
  lives outside the repainted DOM; changing the GPU selection
  invalidates the projection. The flags form's help text now
  points here instead of at the wizard's tensor-split step.
- GPUs page: the editable Placement section is removed. Each GPU
  card lists the models resident on it, read-only: model, serving
  backend, the configured percentage from the backend's split
  (100% when unsplit), and the measured VRAM held — with the
  Service Map residency rule, so a bare CUDA context under 1 GiB
  does not register as a model presence. The per-process table
  remains as the detail underneath.

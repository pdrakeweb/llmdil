# AFK Session — Local LLM Service Manager full wiring

- **Status:** COMPLETE (v0.1.2 released and verified 2026-10-15)
- **Departed:** 2026-10-15 ~02:29 EDT (instruction received via Android app)
- **Expected return:** not stated — run until the queue completes or blocks
- **Remote Control:** unknown — user reachable via this chat (Android app). Conservative mode applies to discretionary medium-stakes decisions not covered by the user's explicit instructions.
- **Council skill:** installed (~/workspace/custom-skills/council) — reviews route there.
- **Watchdog:** this runtime has no PushNotification; the supervisor pattern (subagent completion handoffs + stall checks via subagent.list) serves as watchdog. State changes logged to afk_progress.md in this directory.

## User's instruction (verbatim, 2026-10-15 02:29 EDT)

"After completing the full implementation of every screen mode button and feature, each of these sections to a console using the council skill that we were recently working on, which you can also find under my GitHub user if you need it and have it reviewed and address any findings and also load the AFK skill which you can find under my GitHub user and run all this in AFK mode until you have a completely working piece of software"

## Permission grants (defaults pending user reply)

1. Push to main via gh_sync + publish GitHub release v0.1.2 when all gates pass — DEFAULT: yes, per the user's established pattern for this repo (v0.1.0, v0.1.1 released on his direction the same way). Per AFK §3 this is high tier: if the user replies denying it, the release becomes a BLOCKED item with everything staged.
2. All local work: cargo/npm builds and tests, file edits inside ~/workspace/local-llm-service-manager — granted by the task itself (low tier).

## Work queue (ordered)

1. Backend wiring completion: supervisor spec registration (code written) + src-tauri cargo check.
2. api.ts wrappers for all views.
3. View wiring: Backends, Models, GPUs, Logs, Gateway, Settings (static.ts); Service Map (live data); Dashboard restart button; main.ts integration; UI smoke guards.
4. Full verification: cargo test/clippy/fmt (workspace), cargo check/clippy (src-tauri), UI build + smoke.
5. Council review (review panel, rounds 2, research skipped — internal codebase) per section:
   S1 Tauri detection + backend spec/status wiring (src-tauri), S2 Backends, S3 Models, S4 GPUs/telemetry, S5 Logs, S6 Gateway, S7 Settings, S8 Service Map + Dashboard.
6. Address council findings; re-run affected gates.
7. Version bump to 0.1.2; push; release; install-test follows automatically (release: published trigger); watchdog cron on the release run.

## Reserved / off-limits

- No changes to the nine crates' public APIs unless a council finding requires it (then it is medium tier, councilled as part of that section's review).
- Model SHA-256 placeholders stay fail-closed (known v0.1.x limitation, documented).

# Integration test: desktop app

How to exercise every screen and feature of an installed LLMdil on real
hardware, by hand or with an agent driving the desktop. The same cases
over HTTP are in [integration-test-api.md](integration-test-api.md);
Windows services have their own plan in
[windows-services-test-plan.md](windows-services-test-plan.md).

Every case has an id. Use the ids in results, so two runs can be compared.
A run takes about two hours; the **smoke** subset (cases marked ★) about
thirty minutes.

## 1. What you need

| Need | Why |
|---|---|
| Windows 11, two NVIDIA GPUs, at least one WDDM (drives a display) | Placement, split and paging cases. With one GPU, skip cases marked `2GPU` |
| A GPU without per-process VRAM in nvidia-smi (WDDM) and one with it (TCC) | Measurement cases |
| ≥ 64 GB RAM, ≥ 25 GB free disk | MoE models, the cold backup |
| The catalog models in the table below, downloaded and verified | Test backends |
| Open WebUI (or another OpenAI client) already connected to the gateway | Chat cases. The tester never types or reveals the gateway key |
| A person who can answer Windows admin prompts | Firewall and service cases only |

Catalog models used: `qwen3-8b`, `qwen2.5-coder-7b`,
`nemotron-3.5-lightning-30b` (MoE), any other MoE model (`MOE2` below),
`qwen3.8-27b-mtp`, and one small model that is registered but **not**
downloaded (`SMALL` below).

## 2. Rules

1. **Own objects only.** Everything the test creates is named `qa-*`.
   Existing backends, groups, models, rules, prompts and MCP servers are
   never edited, moved or deleted. Stopping an existing backend to free
   memory is allowed only if the owner agreed.
2. **Check the Activation Plan before starting.** If the owner has pending
   changes, apply them first with their agreement. Cancel restores the
   whole saved configuration, so use Cancel only when every row is yours.
3. **Not part of this test:** typing passwords or API keys, creating users
   or pairing codes, password resets, storing MCP credentials, paid
   OpenRouter, rotating the TLS certificate, revealing the master key,
   log-receiver deploy, `Run setup`, deleting models.
4. **Ask first:** gateway port and network access, MXC mode, component
   updates, model download, Windows services.
5. **Two sources.** A case passes only when the screen and a second source
   agree: `config.json`, `activation-journal.json`, `gateway/config.yaml`,
   the process list, `nvidia-smi`, llama-server `/health` and `/props`, or
   the lifecycle logs.
6. Keep at least 8 GB of RAM available; run one large MoE model at a time.
7. Destructive test actions stop at their confirmation unless the case
   says otherwise.

Record each case as **PASS**, **FAIL** (defect), **ODD** (works, but
confusing) or **SKIP** (with the reason).

### Driving the app with an agent

- Bring the window forward through its title bar or tray icon.
- Take a fresh screenshot after anything that can move the layout, and
  before every click that changes something.
- Native drop-downs may be invisible to the screenshot tool. Open the list
  and use `Home`/`End`/arrow keys and `Enter`; verify the result.
- Filter Backends by `qa-` first; it shows only test objects.
- Type nothing into a field that could hold a secret.

## 3. Prepare

| Id | Step | Expected |
|---|---|---|
| P-1 ★ | Record: app version, build, GPU list, running processes, listening ports, free RAM and disk | Written down |
| P-2 ★ | Copy the data directory without `models` to a backup next to it; copy `llamacpp` too. `robocopy` exit codes 0–7 are success | `config.json` hashes match |
| P-3 | Before a LiteLLM update: stop the gateway, then the database, then copy `postgres` and `python` cold | File count and byte totals match |
| P-4 ★ | Compare the Activation Plan with `activation-journal.json` | Same rows |

## 4. Screens (read-only)

For each screen: open it maximised and at a narrow width, open every tab,
expander, info tip, sort, filter and range, and reload it.

| Id | Screen | Check |
|---|---|---|
| S-1 ★ | Dashboard: Appliance, Console, Topology; pause/resume; trends; overview links; with ≥ 15 backends | No overlap, no sideways page scroll, every node readable, states match the process list |
| S-2 | Service Map | Labels inside their boxes; edges only where they mean something; legend readable |
| S-3 ★ | GPUs: poll rate, refresh, reset peaks, snapshot, filter (incl. no match), ranges, process list | Values match `nvidia-smi`; what "reset peaks" resets is visible; the snapshot file exists |
| S-4 ★ | Backends: sort, state filter, text filter by id, group, file **and displayed model name**; details; menus near the window edge | Menus fully visible; filter and scroll survive leaving and returning |
| S-5 | Models: details, copy buttons, capability form and rename opened then cancelled | Nothing saved |
| S-6 ★ | Logs: each scope, level, search, From/To, sort both ways, paging, limit, follow, recents, folder | Line times are event times, not read times; error lines stand out |
| S-7 ★ | Gateway: Flow and Groups tabs, generated config, rules, OpenRouter panel | Live strategy and group count are the running ones; saved ones are labelled as such |
| S-8 | Groups, MCP Servers, Optimize, Bench, Activation Plan | Render with and without data |
| S-9 | Settings: all tabs | Inputs styled; values match `config.json` |
| S-10 | Setup Wizard: step details, re-run audit | Finished setups don't reopen the wizard at every launch |

## 5. App life cycle

| Id | Step | Expected |
|---|---|---|
| L-1 ★ | With the app open, start it again from the Start menu | The existing window comes forward; still exactly one process |
| L-2 ★ | Close the window, then start it again | Goes to the tray; the same process shows its window |
| L-3 | Tray → Show, tray → Quit | Gateway and database keep serving after Quit; relaunch adopts them |
| L-4 | Relaunch | Opens on the Dashboard when setup is done; window size remembered |

## 6. Create

Groups page, *New group*:

| Id | Input | Expected |
|---|---|---|
| G-1 ★ | `qa-small`, `qa-moe` | Created |
| G-2 | empty; `QA Small!`; an existing name; 63 characters; `openrouter` | Refused; each message names the actual rule (the long name mentions the length) |
| G-3 | Delete an empty group (arm, then confirm) | Gone; a group with members offers no delete |

Backends → *Add backend*:

| Id | Backend | Configuration |
|---|---|---|
| B-1 ★ | `qa-qwen8b` in `qa-small` | `qwen3-8b`, GPU 0 only, 32K, defaults |
| B-2 ★ | `qa-coder7b` in `qa-small` | `qwen2.5-coder-7b`, GPU 1 only, KV q8_0/q4_0, flash auto |
| B-3 ★ | `qa-nemo` in `qa-moe` | `nemotron-3.5-lightning-30b`, GPU 0, partial MoE offload |
| B-4 | `qa-moe2` in `qa-moe` | `MOE2`, GPU 1, default offload |
| B-5 `2GPU` | `qa-dense27` in **new group** `qa-split` | `qwen3.8-27b-mtp`, both GPUs |
| B-6 | `qa-big` in `qa-split`, Enabled **off** | `qwen3.8-27b-mtp`, GPU 0, 262,144 context: shows "Does not fit" |
| B-7 | Custom context `10000`, batch `700`, parallel `3` on a throwaway edit | Fields accept the numbers; llama-server `/props` shows them after a restart |

Refusals on the same form: duplicate name; `QA!`; no GPU; port of another
backend, of the gateway, 0, 70000, empty; context 1023 and 1,048,577;
batch 0 and above context; parallel 0 and 1025; quantised V cache with
flash off; "new group" with an existing name. Expected: refused in plain
words, next to the form, and the message clears when the input is fixed.

Also check: the memory lines follow **every** control (MoE slider, GPUs,
cache types); "fits" uses **free** memory; the caption shows exactly the
flags that will be saved; the MoE slider keeps keyboard focus over several
key presses.

## 7. Activation Plan

| Id | Step | Expected |
|---|---|---|
| A-1 ★ | Toggle one `qa-*` backend | One pending change, naming the backend and what changed |
| A-2 ★ | *Apply now* | Gateway back in about a minute; `config.yaml` has the new groups; history row |
| A-3 ★ | *Drain Requests and Apply* with no traffic | Starts draining and finishes within seconds |
| A-4 | Drain while a long reply streams | Waits for it; if it times out, the result says so |
| A-5 ★ | Edit a **running** `qa-*` backend (context, cache, parallel), then *Cancel changes* (arm, confirm) | Exactly those edits are undone; nothing else pending afterwards (no "drift") |
| A-6 | *Later*, *Review*, refresh, execution details | Work; rows name their targets |
| A-7 | Apply a plan whose backend cannot fit | Reported as failed with the memory reason; other items still applied |

## 8. Run and fail

| Id | Step | Expected |
|---|---|---|
| R-1 ★ | Start `qa-qwen8b`; Test; Test tool calls; launch command; details | Ready in seconds; predicted memory within 0.5 GB of `nvidia-smi`; "ready" |
| R-2 | Click Start twice quickly | Starts once |
| R-3 | Stop while starting; restart; stop | State always matches the process |
| R-4 ★ | Stop the largest model that runs here | Ends as stopped, not failed |
| R-5 ★ | Start a backend larger than the GPU's free memory | Refused: GPU, need, free, total, who uses it. The card doesn't say "Fits" |
| R-6 | `qa-big` on GPU 0 | Refused with the weights/KV/state breakdown |
| R-7 | `qa-big` moved to a WDDM GPU, click Start **once** | Asks to confirm paging; not called "fits"; **don't confirm** |
| R-8 ★ | Backend whose port is taken by another program | Refused quickly, naming the program; no stale log shown |
| R-9 ★ | Kill one `qa-*` llama-server by its pid | Failed, then restarted by its policy with back-off; after too many crashes it stays failed |
| R-10 ★ | `qa-nemo` with partial MoE offload on a compute-capability 6.x GPU | Blocked or warned before start, with the safe positions; full offload answers a prompt |
| R-11 | Two backends on one GPU until it is nearly full | Both answer, or the second is refused |
| R-12 | Backend of `SMALL` before its download | Refused: model file missing |
| R-13 | An invalid flag as text | Warned on entry; start fails with llama-server's message |

## 9. Reconfigure

Change, apply, restart, then check `/props`, the process command line and
`nvidia-smi`: context, batch, parallel, K/V cache, flash attention, MTP
draft length, expert cache, MoE offload 0 / middle / all, flags as text and
back. Then placement: GPU 0 → GPU 1 → both → propose/apply.

| Id | Check | Expected |
|---|---|---|
| C-1 ★ | What saves immediately and what needs Save | Same rule for all controls; X discards unsaved edits |
| C-2 `2GPU` | Split over two GPUs | Both GPUs hold weights; every page lists both |
| C-3 ★ | Move a backend where it does not fit | Placement says it doesn't fit; nothing starts with fewer GPU layers than shown |
| C-4 | Flags as text | Known flags don't warn; context stays visible; discard asks first |
| C-5 | Saving with a stale panel open | Launch command, memory and GPU lines refresh |

## 10. Gateway and chat

| Id | Step | Expected |
|---|---|---|
| W-1 ★ | Refresh the client's model list; chat with `qa-small` (short, code, long, follow-up) | Correct answers; requests spread over the members |
| W-2 ★ | Stop one member, send ten prompts | All ten answered |
| W-3 | Stop both members | A clear error, or the configured fallback |
| W-4 | Group with a fallback | Fallback answers |
| W-5 | Stop a streaming reply | The backend slot becomes idle |
| W-6 ★ | Dashboard, cards and GPU history after traffic | Non-zero throughput; failures counted; counts survive a gateway restart |
| W-7 | Rules: add, negative tokens, second rule, edit, disable, delete | Errors appear at the form |
| W-8 | Fallbacks: unknown, itself, valid | First two refused at the field; third saved |
| W-9 | Move a backend between groups by drag **and** by menu; `Escape` closes the menu | Moved; one net change |
| W-10 | Two groups with different strategies | The page says which one applies and why |
| W-11 | No key, wrong model | 401, 404 |

## 11. Optimize and Bench

Back up and afterwards restore `optimize-*.json` and `bench-*.json`.

| Id | Step | Expected |
|---|---|---|
| O-1 | Purpose edit and cancel; context viewer | Nothing saved |
| O-2 ★ | Estimate, then Measure the same backend | Prompt speeds agree within 2×; first-token time present |
| O-3 | Consult with an existing plan | Asks before replacing it; result is not at once "configuration changed" |
| O-4 | Accept only `qa-*` items; leave the rest undecided; apply | Only those changes land in the plan |
| O-5 ★ | Tuning preview and run on a GPU-0 backend | The current setup is one of the arms |
| O-6 ★ | Cancel a running sweep | Everything it stopped runs again, or the page says what doesn't |
| O-7 | Discover, sample plan, experiments estimate, adopt, discard | Work or explain why not |
| N-1 | Prompt: empty, create, edit, delete, generate, cancel | Dates are real; errors next to the form |
| N-2 ★ | Custom sweep (two arms, depth 0) | Status text names the sweep; progress and Cancel visible at once |
| N-3 | Temperature 5 | Refused |
| N-4 ★ | Sweep on a running backend | Interlock names it; after the run the panel is gone |
| N-5 | Results: metrics, compare, delete own sweep | A sweep without measured arms isn't "Complete" |

## 12. Models, MCP, settings

| Id | Step | Expected |
|---|---|---|
| M-1 | Download `SMALL`: pause, resume, cancel, resume, verify | Percentage and speed shown; confirmation is an app dialog |
| M-2 | *Add model*: empty, duplicate, non-https URL, lookup | Form visible where you clicked; refused in plain words |
| K-1 | MCP: bad id, local + http, http off loopback, valid disabled server, Test, remove | Refused / saved / removed |
| K-2 | A server with a missing credential | Says it cannot start and why |
| T-1 | Telemetry interval out of range | Refused |
| T-2 ★ | Gateway port: in use, then another, start, back | While changing, the page shows the **running** port; old firewall rule removed |
| T-3 | Network access: this computer, then back | Bind address changes after a restart |
| T-4 ★ | MXC self-test and Enforced without the runtime | Says the runtime isn't installed; Enforced refused |
| T-5 | Notifications: master, categories, test | Toast, or the reason there is none |
| T-6 | Supervision mode to Service and back | See the Windows services plan |
| T-7 | Diagnostics export | Zip written; no `sk-` keys, passwords or connection strings inside |

## 13. Updates (owner's agreement)

| Id | Step | Expected |
|---|---|---|
| U-1 | Check; leave the page; return | Result and time still shown |
| U-2 | llama.cpp, if a newer verified build is offered, with only `qa-qwen8b` running | Updated and smoke-tested, or rolled back with the reason |
| U-3 | LiteLLM, after P-3 | Updated or rolled back; W-1 and W-2 pass again |
| U-4 | Pin a version that cannot be installed | Refused |

## 14. Put back and compare

1. Stop and disable every `qa-*` backend (or delete them if asked), apply.
2. Restore settings, the dashboard view and the files named in section 11
   (with the app closed).
3. Compare `config.json` with the backup **field by field**. Allowed
   differences: `qa-*` objects, `assigned_backends` of their models, and
   changes the owner approved.
4. Restart what was running. Check the gateway, and that one app process
   and no leftover llama-server remains.
5. List anything left behind (firewall rules, snapshots, bundles, files).

## 15. Result sheet

```
Build: ____  Date: ____  GPUs: ____  LiteLLM: ____  llama.cpp: ____
Case | Result | Evidence | Note
```

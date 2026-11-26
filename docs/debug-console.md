# Debug console — design and reference

Status: built, 2026-11-26. Crates: `manager-diag` (pure logic) and
`manager-app::console` (state, processes, persistence, gates); UI:
`ui/src/views/console.ts` (route `#/console`). Agent guide:
[`AGENTS.md`](../AGENTS.md).

The console gathers evidence; it does not diagnose. A human in the UI
or an agent over the admin API pastes recommended diagnostic commands,
reviews the recipe LLMdil makes of them, runs it under LLMdil's
control (GPUs freed, environment set, output captured), reads
structured results, and keeps investigating from the same prompt.
Everything the UI does goes through registry operations, so the admin
API offers exactly the same surface.

## 1. Backend output persistence

Process-mode backends used to keep stdout/stderr only in the
supervisor's in-memory ring; a crash's cause was gone after a restart.
Now the supervisor tees captured output to
`logs/backend-<id>.stdout.log` / `.stderr.log` — the same files the
service shim writes in service mode:

- opening a run rotates the previous run's file to `.prev` (matching
  `gateway.stderr.log`);
- a 32 MiB segment cap: past it the segment becomes `<file>.1` and a
  fresh segment continues, so the *tail* (where the crash is) is
  always kept and `.prev` is never touched mid-run;
- an exited process's readers now drain the pipe to EOF (bounded,
  2 s) instead of being aborted, so the last lines a crashing process
  wrote — the CUDA error block — reach the ring and the file.

`get_logs` merges the files only when the ring has no lines for the id
(service mode, adopted, or after an app restart), so nothing shows
twice; the log shipper tails the files in both modes (the ring pump is
gone). The console reads and greps current (`.1` + live) and previous
(`.prev`) runs: `logs <backend> [--prev] [--grep X]`,
`GET /api/v1/console/backends/{id}/logs`.

## 2. Recipes

A recipe (`manager_diag::Recipe`, JSON under
`<data_dir>/diagnostics/recipes/<name>.json`) is a named, ordered list
of steps plus parameters, recipe-level env, and requirements:

```json
{
  "name": "fa-kv-depth-ab",
  "params": [{ "name": "m", "default": "{models_dir}/Whittle-MoE-27B-A18B-v2.2.1-Q4_K_M.gguf" }],
  "env": { "CUDA_LAUNCH_BLOCKING": "1" },
  "requires": { "gpus": null, "stop_backends": [], "keep_running": [], "restore": true },
  "steps": [{
    "name": "llama-bench",
    "exe": "{llamacpp_bin}/llama-bench.exe",
    "args": ["-m", "{m}", "-ngl", "99", "-ot", "exps=CPU", "-p", "512", "-n", "0",
             "-d", "0,4096,8192", "-fa", "1", "-ctk", "q8_0", "-ctv", "q8_0"],
    "parser": "llama_bench",
    "variants": [
      { "name": "-d 0,4096,8192 -fa 1 -ctk q8_0 -ctv q8_0", "expect": "may_crash",
        "description": "expect crash at d=8192" },
      { "name": "-d 8192 -fa 1 -ctk f16 -ctv f16", "set": { "-d": "8192", "-ctk": "f16", "-ctv": "f16" } },
      { "name": "-d 8192 -fa 0 -ctk q8_0 -ctv f16", "set": { "-d": "8192", "-fa": "0", "-ctv": "f16" } }
    ]
  }]
}
```

Step fields: `kind` (`exec` | `http` | `gguf`), `exe`, `args`, `env`,
`workdir`, `timeout_secs` (default 600, max 6 h), `parser`
(`llama_bench` | `server_timings` | `cuda_error` | `raw`), `expect`
(`success` | `may_crash` — a crash is evidence, not a failure |
`may_fail`), `variants`, `background` (+ `ready_url`), `http`,
`gguf`, `bench_json` (append `-o json` to llama-bench, default on).

**Variants** are one step template plus flag overrides (`set`
replaces a flag's first occurrence or appends it; `unset` removes it;
`env` and `expect` per variant). `run --variant N` runs one.

**Templates** (`{name}`; `{{`/`}}` escape): `{llamacpp_bin}`,
`{data_dir}`, `{models_dir}`, `{diagnostics_dir}`,
`{backend.<id>.model_path|port|url|n_ctx|raw_flags|server_args|bench_flags|cuda_visible_devices}`,
`{gpu.<n>.name|uuid}`, `{env.NAME}`, and user parameters. A
parameter whose value is a backend id aliases that backend's
variables: `backend=planner-moe` makes `{backend.model_path}` work.
List variables (`raw_flags`, `server_args`, `bench_flags`) used as a
whole argument splice into several argv entries. An env value that
resolves to empty is not set (an empty `CUDA_VISIBLE_DEVICES` would
hide every GPU).

**Requirements.** GPUs are inferred per step from the resolved argv
and env — `CUDA_VISIBLE_DEVICES` (indices or UUIDs), `-ngl 0` (CPU),
`-dev`, `-ts` (`/` or `,`), `-sm none` + `-mg`; with layers on the GPU
and no selection, every visible GPU — or declared in
`requires.gpus`. Every *live* backend sharing a needed GPU (Bench's
placement judgment; an undetermined placement is assumed to share)
plus `requires.stop_backends` must be stopped; `keep_running` exempts
backends a step talks to.

**Built-ins** (compiled in, cannot be shadowed or deleted):

| name | what |
|---|---|
| `llama-bench-depth` | llama-bench at increasing KV depth with a backend's current KV types / flash-attn / offload flags (`backend=`, `depth=`, `p=`) and its real GPU visibility |
| `gguf-metadata` | GGUF header metadata, in-process (`model=` path) |
| `nvidia-smi-snapshot` | per-GPU memory/util/clocks, compute processes, full report |
| `server-crash-repro` | the backend's exact llama-server argv on a scratch port (real backend drained + stopped, then restored), one long synthetic prompt (`prompt_tokens=`), captures the server's stderr, exit code and CUDA block |

## 3. Paste import

`console_import` (`POST /api/v1/console/import`) parses PowerShell or
cmd into a draft recipe and a plan preview. It never runs anything.

- PowerShell: `$env:X=…` → recipe env; `$m="…"` → parameter `m`
  (`$m`, `${m}`, `$($m)` → `{m}`); `$env:X` inside values → `{env.X}`;
  `.\tool.exe`, `tool`, `& "C:\…\tool.exe"`; `#` comments; backtick
  continuations; `;` separators; `cd`.
- cmd: `set X=…` / `set "X=…"`, `%X%`, `REM` / `::`, `^`, `&&`, `cd /d`.
- Comments become descriptions (a line comment describes the next
  command; an inline comment its own). A comment mentioning a crash
  marks the step `may_crash`.
- Executables resolve against the managed llama.cpp install first,
  then PATH; paths under the data dir become `{data_dir}` /
  `{models_dir}` / `{llamacpp_bin}`. Pipes and redirections are dropped
  with a warning (output is captured per step anyway).
- Consecutive invocations of the same tool with the same positionals
  fold into one step with variants (`fold`, default on); the result
  carries `unfolded` for an Unfold toggle.
- Quoting: `"…"` (with `""` and backtick escapes), `'…'` literal (`''`
  escape). Pasted text never becomes a shell string — every command is
  an argv array.

## 4. Execution

`console_run_start` resolves the plan, refuses on plan errors
(unresolved templates, missing or non-allowlisted executables), and
answers `started: false, needs_confirmation: true, confirm_stop: [...]`
until the caller confirms each backend that must stop (`confirm_stop`,
or `confirm_all`). One run at a time; refused while a Bench sweep runs.

A run (`manager-app/src/console/runner.rs`):

1. drains and stops the backends through `backend_lifecycle`
   (graceful `/slots` drain, operator intent, origin `console`), with
   markers in `logs/backend-<id>.lifecycle.log`;
2. runs the steps one after another as `SupervisedProcess`es (output
   capture, kill-on-drop, bounded kill, below-normal priority, the CUDA
   busy watchdog), streaming every line (`console-output` events and
   `console_run_output` long-poll), enforcing timeouts and cancel,
   sampling GPU telemetry from the shared telemetry store before /
   every second during / after each step; background steps (a scratch
   server) wait for `ready_url` and are stopped when the recipe ends;
3. **always restores** the stopped backends — success, crash, timeout,
   cancel, or a panic in the step task (the steps run in their own
   task; the restore path is outside it) — again with lifecycle
   markers;
4. persists `diagnostics/runs/<yyyymmdd-hhmmss>-<recipe>/`:
   `run.json` (recipe snapshot, resolved commands, env, backend
   stop/restore outcomes, per-step exit, analysis, telemetry, timings,
   summary), `output.jsonl`, `steps/NN.stdout.log` / `.stderr.log`,
   and `export.json` on request.

Run status: `completed`; `completed_with_failures` (every step ran
and its evidence is recorded, but a step expected to succeed did not);
`failed` (the steps could not run: a backend would not stop, or the
runner stopped unexpectedly); `cancelled`.

Analysis per step: NTSTATUS / signal decoding (`0xc0000409` →
"abort() — GGML_ABORT/fast-fail, not memory corruption",
`0xc0000005`, `0xc000001d`, DLL-load codes, …), llama-bench results
(JSON objects survive a crash mid-array; markdown table fallback) with
pp t/s per depth and the failing depth, llama-server timing lines and
the last `prompt processing … progress` line, CUDA/ggml error blocks
(abort site, error, device, function, location, failing call;
`GGML_ASSERT`), and a stderr excerpt around the error. Results table:

| variant | exit | decoded | pp t/s per depth | failing depth | CUDA error excerpt |

Compact results are the default (`GET /api/v1/console/runs/{id}`);
`?full=true` adds raw stdout/stderr (tail-capped at 4 MiB) and
telemetry samples.

## 5. Interactive console

The prompt runs built-ins (`help` lists them): `recipes`, `recipe`,
`plan`, `run <recipe> [--variant N] [-p k=v] [--set FLAG=V] [--stop
a,b | --yes] [--no-restore]`, `cancel`, `import`,
`start|stop|restart <backend>`, `logs <backend> [--prev] [--grep X]
[--tail N]`, `gpu`, `flags <backend>`, `set-flags <backend> <flags…>`,
`bench <backend> --depth … [--fa 0] [--ctk f16]`, `gguf
<model|backend|path> [keys…]`, `history`, `show <run-id> [--full]`,
`export <run-id>`.

`set-flags` writes through `backend_set_raw_flags` (switching the
backend to advanced flags mode first when needed) — journaled in the
activation plan like any other config change, applied on the next
start, undoable through the activation journal.

The **raw shell** is a separate mode: the Console's *Raw shell* toggle
opens a `pwsh` session (Windows PowerShell fallback) for that console
session only; lines go to it instead of the built-ins. It is
line-oriented over pipes (no ConPTY: plain text is what the pane and
agents can read; interactive TUI programs are out of scope). Off by
default; every line sent is audited.

## 6. Agent access

All operations are registry ops with REST routes (Tauri commands of
the same names):

| route | op | class |
|---|---|---|
| `GET /api/v1/console/recipes` | `console_recipes_list` | read |
| `GET /api/v1/console/recipes/{name}` | `console_recipe_get` | config-read |
| `POST /api/v1/console/recipes` | `console_recipe_create` | config |
| `PUT /api/v1/console/recipes/{name}` | `console_recipe_update` | config |
| `DELETE /api/v1/console/recipes/{name}` | `console_recipe_delete` | config |
| `POST /api/v1/console/import` | `console_import` | config |
| `POST /api/v1/console/plan` | `console_plan` | read |
| `POST /api/v1/console/runs` | `console_run_start` | destructive (admin) |
| `POST /api/v1/console/runs/{run_id}/cancel` | `console_run_cancel` | destructive |
| `GET /api/v1/console/runs` | `console_runs_list` | read |
| `GET /api/v1/console/runs/{run_id}` | `console_run_result` | read |
| `GET /api/v1/console/runs/{run_id}/output` | `console_run_output` (`since`, `wait_ms` ≤ 25 s long-poll) | read |
| `GET /api/v1/console/runs/{run_id}/export` | `console_run_export` | read |
| `POST /api/v1/console/exec` | `console_exec` (each command checks its own class) | read |
| `GET /api/v1/console/backends/{id}/logs` | `console_backend_logs` | read |
| `GET /api/v1/console/settings` | `console_settings_get` | config-read |
| `POST /api/v1/console/shell/{session_id}` (+`/input`, `/output`, `DELETE`) | `console_shell_*` | destructive |
| — (desktop only) | `console_settings_set` | security |

Live output also arrives on `GET /api/v1/events` as `console-output`
(batches of lines) and `console-run` (status) events.

**MCP:** LLMdil does not expose an MCP server of its own (`manager-mcp`
manages *other* MCP servers; the delegation MCP server is a separate
project). Exposing the console as MCP tools is a follow-up: a thin
stdio MCP server wrapping these REST routes (import, plan, run, output
long-poll, result, exec, logs) with the same API token.

## 7. Security

- **Loopback gate.** The admin API binds every interface. Recipe runs,
  `bench`, lifecycle commands, `set-flags` and the raw shell are refused
  (403) for non-loopback requests unless *Allow remote execution*
  (`diagnostics.allow_remote_exec`) is on. Reads are not gated.
- **Who.** Every op that needs to know takes a `caller` argument the
  REST layer always overwrites from the authenticated principal, its
  role and the peer address — a client cannot spoof it. The desktop
  passes none and is the local operator.
- **Roles.** Runs and the shell are admin (`D`); lifecycle commands
  operator; `set-flags` configurator; reads everyone.
- **What.** Recipe steps run only allowlisted executables
  (`diagnostics.exec_allowlist`; default `{llamacpp_bin}/*` and
  `nvidia-smi`, the latter only as PATH resolves it). Script files
  (`.bat`, `.cmd`, `.ps1`, …) are refused outright; `..` is refused in
  executable paths; `http` steps may only address loopback. The raw
  shell over the API needs `diagnostics.api_raw_shell`. Settings change
  only in the desktop app.
- **Audit.** Imports, recipe changes, runs (with resolved command
  lines and env), every console line, shell lines and refusals go to
  `logs/admin-api-audit.jsonl` with the actor (`desktop-ui` or the API
  principal), source and request id — through the same process-wide
  writer as the API's own audit records.

## 8. Out of scope

Remote / multi-machine execution and automatic diagnosis. Known
limits: one run at a time; an app exit mid-run leaves stopped backends
stopped (the run record lists them); the raw shell has no terminal
emulation.

# AGENTS.md — operating LLMdil's debug console

How an agent (Claude, or any client of the admin API) should
investigate a backend problem with the debug console. Design and
reference: [`docs/debug-console.md`](docs/debug-console.md).

## Ground rules

- Talk to the admin API (default `https://<host>:47821`, bearer token)
  **from the LLMdil machine itself**: anything that runs processes is
  refused for non-loopback requests unless the operator turned on
  *Allow remote execution*. Reads work from anywhere.
- Use the **built-ins and recipes**, not a shell. The raw shell is off
  for the API unless the operator enabled it; recipe steps run only
  allowlisted executables (managed llama.cpp binaries, `nvidia-smi`).
- Everything you do is audited under your principal.
- The console gathers evidence. Reason about it yourself; don't expect
  it to diagnose.

## The loop

1. **Look first** (cheap, read-only):
   ```
   POST /api/v1/console/exec {"line": "flags planner-moe"}
   POST /api/v1/console/exec {"line": "logs planner-moe --prev --grep \"CUDA error\""}
   POST /api/v1/console/exec {"line": "gpu"}
   POST /api/v1/console/exec {"line": "history 5"}
   ```
   Every `exec` returns `{ok, text, data}` — `text` for reading,
   `data` structured.

2. **Import** the commands you would recommend (PowerShell or cmd):
   ```
   POST /api/v1/console/import {"text": "<the block>", "name": "fa kv depth ab"}
   ```
   You get `recipe` (folded into variants when the invocations differ
   only in flags), `unfolded`, `folds`, `warnings`, and `plan`: the
   resolved command lines, env, GPUs needed, and backends that would be
   stopped. Nothing ran.

3. **Review and fix** the draft: read `warnings` and `plan.errors`;
   edit the recipe JSON if needed (set `expect: "may_crash"` on a
   variant you expect to crash, so the crash is recorded as evidence).
   Re-check with `POST /api/v1/console/plan {"recipe_doc": {...}}`.
   Save it if you will reuse it: `POST /api/v1/console/recipes
   {"recipe": {...}}`.

4. **Run**:
   ```
   POST /api/v1/console/runs {"recipe": "fa-kv-depth-ab"}
   ```
   If backends share the GPUs, the answer is `started: false,
   needs_confirmation: true, confirm_stop: ["planner-moe"]` — repeat
   with `"confirm_stop": ["planner-moe"]` once that is acceptable.
   They are drained, stopped, and restarted when the run ends (also
   after a crash or a cancel). Pass `"variant": N` to run one variant,
   `"params": {...}` for parameters, `"set": {"--parallel": "1"}` to
   override a flag on every exec step.

5. **Follow** without pulling megabytes:
   ```
   GET /api/v1/console/runs/{run_id}/output?since=0&wait_ms=20000
   ```
   Long-polls up to 25 s; pass the returned `next` as `since`. Stop
   when `done` is true and `more` is false. Or just wait and go to 6.

6. **Read results** (compact by default):
   ```
   GET /api/v1/console/runs/{run_id}
   ```
   `summary.table` is the per-variant comparison (exit, decoded
   NTSTATUS, pp t/s per depth, failing depth, CUDA error excerpt);
   each step has `analysis` (parsed results, `cuda_errors`,
   `stderr_excerpt` around the error) and `telemetry` (before / after /
   peak per GPU). Ask for `?full=true` only when the excerpt is not
   enough.

7. **Iterate** from the same console:
   ```
   POST /api/v1/console/exec {"line": "run fa-kv-depth-ab --variant 1 --set --parallel=1 --yes"}
   POST /api/v1/console/exec {"line": "run server-crash-repro backend=planner-moe prompt_tokens=9000 --set --parallel=1 --yes"}
   POST /api/v1/console/exec {"line": "gguf planner-moe attention.key_length head_count_kv"}
   POST /api/v1/console/exec {"line": "bench planner-moe --depth 4096,6144,8192 --fa 0 --yes"}
   ```
   A configuration change you want to keep goes through
   `set-flags <backend> <flags…>` — journaled in the activation plan,
   applied on the next start (`restart <backend>`), undoable.

## Worked example

`planner-moe` crashes with `CUDA error: invalid argument` in
`ggml_cuda_kernel_launch` and exit `0xc0000409` around 8k tokens of KV.
Import Claude's three `llama-bench` lines; the console folds them into
one step with three variants, resolves `$m`, applies
`CUDA_LAUNCH_BLOCKING=1` to each, sees the run needs GPU 0, and asks to
stop `planner-moe`. Confirm, run, and read the table: variant 1 dies at
depth 8192 with `0xc0000409` = "abort() — GGML_ABORT/fast-fail, not
memory corruption" and the CUDA block; the f16-KV and FA-off variants
complete. Follow up with `server-crash-repro` on the real flags and a
GGUF metadata dump of the attention geometry.

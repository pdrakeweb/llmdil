# Delegation MCP Server — Design

Status: design, 2026-10-17. Local tree only; nothing here is implemented.
Companion documents: `docs/mcp-capabilities-design.md` (gateway-native MCP for
models served *by* Prometheus — a different surface, see §2.4) and the model
research evidence file at `~/workspace/.tmp-assets/delegation-design/model-research.md`.

**Implementation status (2026-10-17):** the Delegation MCP server
was built the same day this design was written, but **not in this
repository** — it is a separate, standalone Rust project (built on
the `rmcp` MCP SDK) living outside the LLMdil tree, per the §2.3
placement decision (the server runs on the client machine, hosted
by the chat client). Nothing in this repo implements it: there is
no delegation code in `crates/`, `src-tauri/`, or `ui/`, and no
LLMdil release ships it. This document remains the design record
for that external project; the gateway-side preconditions it
references (§2.5 — `--jinja`, the `backend_test_tool_call` probe)
did ship in this repo as part of the gateway MCP work.

How to read the labels in this document: **VERIFIED** = taken from a primary
source (Hugging Face model cards / API file metadata, llama.cpp documentation)
on 2026-10-17. **ESTIMATE** = arithmetic or inference, reasoning shown, never a
measurement. **DERIVED** = exact arithmetic on VERIFIED values. Model benchmark
numbers are vendor model-card numbers at full precision; quantized builds score
somewhat lower.

---

## 1. Intent

Pete wants to learn agentic architecture by building and running a real one,
not by reading about one. The system this document designs has three actors:

- **An orchestrator**: one large reasoning model in a desktop chat client
  (Chatbox). It talks to Pete, plans, decomposes work, and synthesizes final
  answers. It does not touch files or run commands itself.
- **A Delegation MCP server**: a small program running on Pete's dev machine,
  hosted by the chat client. It exposes *delegation tools* to the orchestrator.
  Each delegation runs an internal **sub-loop**: the server drives a specialist
  model through Pete's existing LiteLLM gateway, gives that specialist a set of
  local tools executed on the client machine, feeds tool results back until
  the subtask ends, and returns one bounded result to the orchestrator.
- **Specialists**: existing gateway groups with distinct jobs — code
  implementation goes to `coder`, minor bug fixes and reformats to
  `coder-fast`, tool/command tasks to `tool-runner`.

Two design commitments shape everything below:

1. **Tool execution happens on the client machine, never on the Prometheus
   host.** The gateway is a stateless inference pipe. All agent logic — the
   outer loop in Chatbox, the sub-loop in this server — lives on the client.
2. **Containment lives in the server.** Chatbox has no per-call approval
   prompt in chat mode (verified 2026-10-17), so there is no human in the
   loop between a specialist's decision and a file write. The server's tool
   implementations, workspace enforcement, and loop caps are therefore safety
   mechanisms, not conveniences.

This document teaches as it goes: each mechanism is explained with its
tradeoffs, because Pete intends to modify this system as he learns.

---

## 2. Architecture

### 2.1 Terms, defined once

- **MCP (Model Context Protocol):** a JSON-RPC protocol between a host
  application and external tool servers. Here it runs over stdio (the server
  is a child process; JSON-RPC travels on its stdin/stdout).
- **MCP host:** the application the user interacts with — Chatbox. It owns the
  chat session, holds the conversation with the orchestrator, discovers MCP
  tools, and renders tool calls and results in its UI.
- **MCP client:** the protocol client inside the host, one per connected MCP
  server. It calls `tools/list` at startup and `tools/call` when the
  orchestrator emits a tool call.
- **MCP server:** a separate process that exposes tools — here, the Delegation
  server. It is not the gateway and it is not a model.
- **Tool call:** a structured request from a model to run a named function
  with JSON arguments. The model emits it; it executes nothing itself.
  Something else — host, gateway, or in this design the Delegation server —
  executes it and returns a result.
- **Tool (OpenAI sense):** when calling a model through
  `/v1/chat/completions`, the caller declares callable functions in a `tools`
  array (name, description, JSON Schema). The model may answer with
  `message.tool_calls` instead of text. The sub-loop (§4) uses OpenAI tools
  for the specialist; the outer surface (§3) uses MCP tools for the
  orchestrator. The shapes rhyme; the transports differ.
- **Agent loop / sub-loop:** the repeated cycle — send messages to a model →
  model emits tool calls → execute them → append results as messages → send
  again — until the model stops calling tools, declares itself blocked, or a
  cap fires. The *outer loop* is Chatbox driving the orchestrator. The
  *sub-loop* is the Delegation server driving a specialist. They are nested,
  independent loops with independent message histories.
- **Context:** the exact list of messages sent to a model on one request.
  Context is finite (the backend's context window), it is the model's entire
  memory of the task, and every token is re-processed on every loop iteration.
  Who puts what into whose context is the central design decision in §4.3.
- **Specialist:** the model the sub-loop drives for one delegation, selected
  by role (§5). It never talks to Pete and never sees the Chatbox conversation
  except through the task spec.
- **Escalation:** the sub-loop giving up in a structured way — returning
  `blocked` or `failed` with evidence — so the orchestrator can decide what
  to do next. Escalation is a normal outcome, not a crash (§6.4).
- **Task spec:** the bounded input the orchestrator passes to a delegation
  tool: goal, acceptance criteria, hints, constraints. Defined in §4.3.

### 2.2 The diagram

```
Pete
  |  chat
  v
Chatbox (MCP host, Windows dev machine)
  |-- outer loop --> Gateway :4000, model=<orchestrator group>
  |                     ^  returns tool_calls naming delegation tools
  |-- MCP stdio ----> Delegation MCP server (client machine)
                          |-- sub-loop --> Gateway :4000, model=<role's group>
                          |                   ^  returns tool_calls naming local tools
                          |-- executes local tools on the client machine
                          |     (file read/write, cargo, allowlisted commands)
                          |-- returns ONE bounded result to the orchestrator
                          v
                     transcript log on client disk (§4.6)
```

Addresses: gateway LAN base `http://192.168.5.23:4000/v1` (loopback
`http://127.0.0.1:4000/v1` on the host itself), master-key auth. Gateway group
names are used as the `model` field in chat completion requests; LiteLLM
resolves a group to its backend(s). The Delegation server never addresses a
llama.cpp backend directly — bypassing the gateway would bypass Pete's
routing, group enable/disable state, and any future key/budget controls, for
zero benefit.

### 2.3 Why the sub-loop lives in a server, not in the client or the gateway

Chatbox (like the other clients surveyed) runs one model per session and
executes the tools it is given; it has no facility for "run a second, smaller
model in a loop behind this tool call." The gateway could theoretically host
agent logic, but that would put tool *execution* on the Prometheus host,
which Pete has ruled out, and would couple agent behavior to gateway
releases. A small client-side server is the only placement that satisfies
both constraints — and it has a teaching advantage: the entire sub-loop is
one inspectable program Pete owns, with its full transcript on his disk
(§4.6).

### 2.4 Relationship to the gateway MCP design

`docs/mcp-capabilities-design.md` designs MCP servers (Brave search, fetch)
exposed *through the gateway* for models Prometheus serves. This document's
server is exposed *through Chatbox* to the orchestrator, and happens to drive
models through the gateway. The two surfaces compose (a specialist could one
day be offered a gateway MCP tool inside its sub-loop) but neither depends on
the other. Keeping them distinct matters: conflating "tools for served
models" with "a client-side agent harness" is the same category error the
council rejected in the earlier blueprint.

### 2.5 Precondition: specialists must emit structured tool calls

A sub-loop is only useful against a backend that reliably returns parsed
`tool_calls` (not tool-call markup leaked into text). The per-backend probes
for exactly this — explicit `--jinja`, filtering `--no-jinja`, a
`backend_test_tool_call` probe, and a gateway passthrough check — are
specified in `mcp-capabilities-design.md` Phase 0 (§7 there). This design
references that work and gates on it at runtime (§6.5) rather than
duplicating it. The same applies to the orchestrator model itself (§8.5).

---

## 3. Tool surface decision

### 3.1 The two options

- **Option G — generic:** one MCP tool, `delegate(role, task, …)`, with
  `role` an enum: `code_implementation | quick_fix | tool_task`.
- **Option R — per-role:** three MCP tools with distinct names:
  `delegate_code_implementation`, `delegate_quick_fix`, `delegate_tool_task`.

Both run identical machinery underneath: one internal `delegate(role, …)`
function driven by role configuration (§7.4). The question is purely what
the orchestrator sees and emits — and therefore what Pete sees, and what can
go wrong at the selection step.

### 3.2 Decision: one tool per role

**Option R is adopted.** The deciding factor is transcript visibility, which
for a learning user outweighs everything else.

Chatbox renders MCP tool calls in the conversation by tool name, with
arguments expandable. Under Option G every delegation reads identically —
`delegate  role="code_implementation"  task="…"` — and the one judgment the
orchestrator makes at this layer (which role?) is buried inside an argument
Pete must open each call to see. Under Option R the transcript itself is the
lesson:

```
delegate_code_implementation  task="add retry logic to fetch…"
delegate_quick_fix            task="fix the clippy warning in…"
delegate_tool_task            task="run cargo test and report…"
```

Role selection is visible at a glance, searchable, and countable. A bad
habit — the orchestrator routing everything to `delegate_code_implementation`
and never using `quick_fix` — is obvious in one screenshot under R and
invisible without argument-diving under G.

Supporting reasons:

- **Schemas stay honest.** The roles do not want the same inputs (§3.3). A
  generic schema accumulates role-conditional optional fields whose
  conditionality can only be expressed in prose descriptions ("only for
  role=quick_fix…"). Models are measurably worse at respecting prose
  conditionals in a shared schema than at filling a dedicated schema where
  irrelevant fields do not exist. Per-role schemas also permit strict
  validation per role.
- **Selection accuracy.** Tool selection is a match between the task in
  context and the tool names + descriptions + schemas offered. Under R the
  name alone carries the taxonomy; the model makes a 3-way choice among named
  tools, then fills a small schema. Under G the same 3-way choice happens
  inside an argument, where a malformed `role` is a validation error; under
  R, choosing a nonexistent role is unrepresentable.
- **Failure attribution.** When a delegation goes wrong, R makes "wrong tool,
  or right tool with a bad spec?" answerable from the transcript.
- **Descriptions are role-specific and short** under R, because each tool has
  its own description slot. Under G one description must explain all three
  roles and when to pick each — longer, denser, competing for the
  orchestrator's attention on every turn.

Costs, stated honestly:

- Adding a fourth role means adding a fourth tool and restarting the server
  so Chatbox re-runs `tools/list`. Roles are config-driven (§7.4), so this
  is "edit config, restart" — acceptable for a personal system.
- Three small schemas cost more orchestrator context than one medium schema
  (~750 vs ~450 tokens per request, ESTIMATE from the sketches below).
  Negligible against a large-MoE context window.
- Boundary cases must commit to one tool. That is a feature: the commitment
  is visible, and the result contract (§4.5) lets a specialist escalate if
  the role was wrong (§6.4).

### 3.3 Tool schemas

Common field definitions (each schema is self-contained on the wire; MCP has
no schema inheritance):

- `task` — the goal in the orchestrator's own words. Must be self-contained:
  the specialist does not see the Chatbox conversation.
- `acceptance_criteria` — independently checkable conditions ("`cargo test
  -p net` passes", "no new clippy warnings").
- `context_hints` — paths or symbols the orchestrator believes are relevant.
  Hints, not contents — the sub-loop reads files itself (§4.3).
- `workspace_subdir` — optional path relative to the configured workspace
  root, scoping this delegation. Defaults to the root.

```json
{
  "name": "delegate_code_implementation",
  "description": "Delegate a substantive coding task to the coder specialist (gateway group: coder). The specialist works autonomously in the workspace: reads files, writes code, runs build/tests, iterates until acceptance criteria pass or it escalates. Use for new features, multi-file changes, and refactors. Expected scope: several files. Returns a bounded summary, not a transcript.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "task": { "type": "string", "minLength": 20,
        "description": "Self-contained goal. The specialist does not see this conversation; include the relevant function/module names and intent." },
      "acceptance_criteria": { "type": "array", "items": { "type": "string" }, "minItems": 1,
        "description": "Checkable conditions, e.g. 'cargo test -p http passes'." },
      "context_hints": { "type": "array", "items": { "type": "string" },
        "description": "Paths or symbols likely relevant, e.g. 'src/net/fetch.rs'. Contents are gathered by the specialist." },
      "workspace_subdir": { "type": "string",
        "description": "Optional subdirectory of the workspace root to scope file access." },
      "verify_command": { "type": "string",
        "description": "Optional allowlisted verification command to run before declaring success, e.g. 'cargo test -p http'." }
    },
    "required": ["task", "acceptance_criteria"],
    "additionalProperties": false
  }
}
```

```json
{
  "name": "delegate_quick_fix",
  "description": "Delegate a small, well-scoped correction to the coder-fast specialist (gateway group: coder-fast): a failing test with an obvious locus, a rename, a clippy/rustc warning, a one-function bug. Expected scope: 1-2 files, no design decisions. If the fix turns out not to be small, the specialist escalates rather than expanding scope. For anything larger use delegate_code_implementation.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "task": { "type": "string", "minLength": 20,
        "description": "Self-contained description of the defect and its location if known." },
      "acceptance_criteria": { "type": "array", "items": { "type": "string" },
        "description": "Optional; defaults to 'the named check/test passes and no other test regresses'." },
      "context_hints": { "type": "array", "items": { "type": "string" } },
      "workspace_subdir": { "type": "string" },
      "max_files_changed": { "type": "integer", "minimum": 1, "maximum": 5, "default": 2,
        "description": "Hard hint enforced by the sub-loop: exceeding it escalates instead of proceeding." }
    },
    "required": ["task"],
    "additionalProperties": false
  }
}
```

```json
{
  "name": "delegate_tool_task",
  "description": "Delegate a command-and-observe task to the tool-runner specialist (gateway group: tool-runner): run builds/tests/benchmarks, inspect repo state, search the codebase, run an allowlisted diagnostic — and report evidence. This specialist CANNOT modify files. Use it to gather facts before deciding, or to verify state after an implementation. Returns structured evidence, not raw logs (logs are on disk, path included in the result).",
  "inputSchema": {
    "type": "object",
    "properties": {
      "task": { "type": "string", "minLength": 10,
        "description": "What to run or inspect and what question the evidence should answer." },
      "acceptance_criteria": { "type": "array", "items": { "type": "string" }, "minItems": 1,
        "description": "What counts as an answered question, e.g. 'report pass/fail counts for the workspace test suite'." },
      "commands_hint": { "type": "array", "items": { "type": "string" },
        "description": "Optional suggested allowlisted commands, e.g. 'cargo test --workspace'. The specialist may choose others." },
      "workspace_subdir": { "type": "string" }
    },
    "required": ["task", "acceptance_criteria"],
    "additionalProperties": false
  }
}
```

Notes:

- `additionalProperties: false` everywhere. A learning system should fail
  loudly on a misspelled field rather than silently drop it; the validation
  error returns to the orchestrator as an MCP error result and it retries
  with a corrected call — itself a visible, instructive transcript event.
- `minLength` on `task` is a cheap guard against content-free delegations
  ("fix it").
- No `role` field (the tool name *is* the role) and no model field: role →
  gateway group mapping is server configuration (§7.4), not something the
  orchestrator can override per call. The orchestrator's job is task
  decomposition, not model operations.

**AMENDMENT — 2026-10-17 (Pete): the `deployment` argument.** Each of the
three schemas above also carries one optional property not shown in the
originals: `"deployment": { "type": "string" }` — the NAME of a configured
deployment (§7.2), omitted to use the config's default deployment. It is
the only addressing a caller may supply: a raw gateway URL is never
accepted anywhere in the tool surface (a per-call URL is a constant the
model would have to reproduce verbatim, and it would let a caller aim the
server's master key at an arbitrary endpoint). An unknown name returns a
clean error listing the configured names; there is no fallback, and no
HTTP request is made. The effective deployment name and base URL are
logged in every transcript (§4.6) and returned in every result (§4.5).

---

## 4. Sub-loop specification

### 4.1 One delegation, end to end

1. **Outer call.** The orchestrator (via Chatbox) emits an MCP `tools/call`
   for one of the §3.3 tools. From Chatbox's perspective this is one blocking
   tool call; it may take minutes. (How Chatbox behaves under long MCP calls —
   timeouts, progress rendering — is an open validation item, §10 Phase 0.
   The server must finish or fail within its own caps regardless, §6.1.)
2. **Validation and setup.** The server validates arguments against the
   role's schema, resolves the role config (gateway group, system prompt,
   local tool set, caps), canonicalizes and checks the workspace scope
   (§7.3), opens a transcript file (§4.6), and — if the workspace is a git
   repo — records pre-run state (`git status --porcelain`, HEAD) for the
   result's change accounting (§4.5).
3. **Loop initialization.** The server builds the specialist's initial
   message list: one `system` message (role system prompt, §5.3, plus
   server-injected facts: workspace root, OS/shell, date, role caps in plain
   language) and one `user` message containing the rendered task spec
   (§4.3). No Chatbox history is included — ever.
4. **Model call.** The server POSTs to `{gateway_base}/v1/chat/completions`
   with `model` = the role's group name, the message list, the role's local
   tools in the OpenAI `tools` array (§4.2), and per-request bounds
   (`max_tokens`, §6.1). Auth: `Authorization: Bearer <master key>` from the
   server's environment (§7.2). Streaming is OFF for the sub-loop in v1: the
   server needs the complete `tool_calls` structure before executing
   anything, and non-streamed responses make transcript logging and
   parse-failure handling (§6.5) far simpler. Liveness is borne by a log file
   Pete can tail, not by the Chatbox UI.
5. **Branch on the response.**
   - `finish_reason == "stop"`, no `tool_calls`: the specialist considers
     itself done. The server checks the claim (§4.4) — for code roles, a
     "done" without a passing verification run in the transcript is not
     accepted at face value.
   - `tool_calls` present: the server appends the assistant message verbatim,
     executes each call against the local tool implementations (§5.2),
     appends one `tool` message per call (§4.2), and returns to step 4.
   - Unparseable as tool calls: repair path, §6.5.
6. **Termination.** Verified completion, specialist-declared block, any cap
   (§6.1), no-progress detection (§6.3), scope guard, or fatal error (§6.5).
   In all cases the server assembles the result contract (§4.5) — there is
   no exit path that returns nothing, or a raw exception, to Chatbox.
7. **Return.** The result contract goes back as the MCP tool result. Chatbox
   appends it to the orchestrator's context, and the outer loop continues:
   the orchestrator reads a bounded summary, not the sub-loop's history.

Concurrency: **v1 runs one delegation at a time per server process.** A
second concurrent delegation call is queued or rejected with a structured
"busy" error the orchestrator can retry. Parallel sub-loops multiply every
hard problem in §6 (caps, workspace write conflicts, transcript
interleaving) and are a later decision, not an accident of implementation.

### 4.2 Message shapes

The sub-loop speaks the OpenAI chat completions dialect to the gateway —
that is what LiteLLM exposes and what llama.cpp backends consume. The API is
stateless: the full history and tool list are resent on every iteration,
which is why context discipline (§4.3) matters.

**(a) Tool declaration** (one per local tool, sent on every request):

```json
POST /v1/chat/completions
{
  "model": "coder",
  "messages": [ "…history…" ],
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "read_file",
        "description": "Read a UTF-8 text file inside the workspace. Returns numbered lines. Large files are truncated with a marker; use offset/limit to page.",
        "parameters": {
          "type": "object",
          "properties": {
            "path": { "type": "string", "description": "Path relative to workspace root." },
            "offset": { "type": "integer", "minimum": 1 },
            "limit": { "type": "integer", "minimum": 1, "maximum": 2000 }
          },
          "required": ["path"],
          "additionalProperties": false
        }
      }
    }
  ],
  "tool_choice": "auto",
  "max_tokens": 4096,
  "temperature": 0.2
}
```

`tool_choice: "auto"` is the default posture; the verification nudge (§4.4)
is the one exception that may force a final iteration's choice.

**(b) Assistant tool-call message** — returned by the specialist, appended
verbatim to history:

```json
{
  "role": "assistant",
  "content": "I'll look at the fetch implementation first.",
  "tool_calls": [
    {
      "id": "call_9f2c",
      "type": "function",
      "function": { "name": "read_file", "arguments": "{\"path\": \"src/net/fetch.rs\"}" }
    }
  ]
}
```

Note `arguments` is a JSON *string*, not an object — the OpenAI convention,
and the single most common source of parse failures (§6.5). The server parses
it defensively and never assumes well-formedness.

**(c) Tool result message** — one per tool call, keyed by `tool_call_id`:

```json
{ "role": "tool", "tool_call_id": "call_9f2c",
  "content": "1\tuse std::time::Duration;\n2\t\n3\tpub fn fetch(…)\n… (212 lines total)" }
```

Errors are also `tool` messages whenever the specialist can plausibly
recover (§6.5): `ERROR: file not found: src/net/fetch.rs (workspace root
contains: src/, Cargo.toml, …)`. Error text is written for a model reader:
state what failed, plus a fact that helps the next attempt. A bare "ENOENT"
teaches nothing and burns an iteration.

### 4.3 Context passing: the task spec, and who gathers file context

The task spec is the rendered form of the delegation arguments, delivered as
the sub-loop's initial `user` message with a fixed structure:

```
TASK
<task text from the orchestrator>

ACCEPTANCE CRITERIA
- <criterion 1>
- <criterion 2>

CONTEXT HINTS (paths/symbols the orchestrator believes are relevant;
verify them, they may be stale)
- <hint 1>

CONSTRAINTS (server-injected, not orchestrator-supplied)
- Workspace root: <abs path>; all file access is inside it.
- Role: code_implementation. Iteration budget: 25. Wall-clock budget: 20 min.
- Do not modify: <server config denylist, e.g. .git/, target/>
- When you believe the criteria are met, run the verification command
  and then reply with a final summary and no tool calls.
```

Deliberately absent: conversation history, Pete's broader goals, prior
delegations' transcripts. If a fact matters to the subtask, the orchestrator
must put it in `task` — the schema's "self-contained" wording exists to
force exactly that.

**File context is gathered by the sub-loop, with its own local tools — not
supplied by the orchestrator.** This is load-bearing; three reasons:

1. **Locality makes it cheap and current.** The files sit next to the server.
   The specialist can `search_code`, `read_file`, and `list_dir` in
   milliseconds and always sees actual current bytes — including Pete's
   uncommitted edits, which an orchestrator summarizing from an earlier
   reading would miss or misremember.
2. **It protects the orchestrator's context.** File contents are the
   bulkiest objects in any coding task. If the orchestrator read files itself
   and pasted them into specs, every delegation would cost the outer loop
   thousands of tokens in both directions, and the orchestrator's remaining
   budget for planning and synthesis would shrink. The intended division of
   labor: the orchestrator holds *the conversation and the plan*; the
   specialists hold *the code*. The bounded result contract (§4.5) keeps the
   return direction small; self-gathered context keeps the outbound
   direction small.
3. **It matches how the tools work anyway.** A specialist that receives
   pasted contents still needs to write precise edits against real files and
   run real tests. Read tools from the start remove an entire class of
   "the pasted copy was stale/truncated" failures.

The cost, honestly: the specialist spends some iterations on orientation
that a perfect spec could have saved, and sequential delegations re-gather
overlapping context. On local models the cost is time, not API spend, and
the orientation steps are instructive in the transcript. A middle option —
orchestrator attaches full file contents as an optional spec field — was
considered and rejected for v1: it reintroduces failure mode (2) by
invitation, and `context_hints` covers the legitimate need ("start here").

### 4.4 Termination and the verification gate

The loop terminates when any of these fires (checked after every model
response and every tool execution):

1. **Final text answer, no tool calls** — the specialist's declared
   completion or block. Mapped to `succeeded` or `blocked` by §4.5 rules
   only after the verification check below.
2. **Verification gate (code roles).** If the specialist declares success
   but the transcript contains no successful verification run (`cargo test`,
   `cargo check`, or the spec's `verify_command` exiting 0) *after its last
   file edit*, the server does not accept completion. It appends one
   synthetic user message — "You declared completion without a passing
   verification run after your last edit. Run the verification now, or reply
   BLOCKED explaining why it cannot pass." — and allows up to 2 such nudges,
   counted against the iteration cap. This single mechanism prevents the
   most common local-model failure: confident, unverified "done."
3. **Caps** (§6.1). 4. **No-progress detection** (§6.3). 5. **Scope guard**
   (quick-fix file count; any write outside the workspace scope is an
   immediate fatal, §7.3). 6. **Fatal errors** (§6.5).

### 4.5 Result contract

The result is the only thing the orchestrator ever sees of a sub-loop. It
must be small enough to protect the outer context, complete enough that the
orchestrator can act without re-delegating just to find out what happened,
and honest enough that failures are usable. Pete also reads these in the
Chatbox transcript; they double as his window into the system.

Returned as the MCP tool result's text content: one JSON object.

```json
{
  "status": "succeeded",
  "role": "code_implementation",
  "gateway_group": "coder",
  "task_echo": "Add retry logic to the fetch function and make the tests pass",
  "summary": "Added exponential-backoff retry (max 3 attempts, 200ms base, jitter) around the request in fetch() in src/net/fetch.rs. Transient errors (connect, 5xx) retry; 4xx returns immediately. cargo test -p net: 42 passed, 0 failed.",
  "acceptance": [
    { "criterion": "cargo test -p net passes", "met": true,  "evidence": "test result: ok. 42 passed; 0 failed" },
    { "criterion": "retries transient failures up to 3 times", "met": true, "evidence": "new test fetch_retries_on_503 passes" }
  ],
  "files_changed": [
    { "path": "src/net/fetch.rs", "change": "modified", "lines_added": 38, "lines_removed": 6 },
    { "path": "src/net/fetch_tests.rs", "change": "modified", "lines_added": 24, "lines_removed": 0 }
  ],
  "verification": {
    "command": "cargo test -p net",
    "exit_code": 0,
    "summary": "42 passed; 0 failed; 1 ignored",
    "ran": true
  },
  "attempts": { "iterations": 9, "tool_calls": 14, "wall_clock_s": 214, "nudges": 0, "repairs": 0 },
  "blocked_reason": null,
  "notes": ["Noticed: src/net/dns.rs has a similar hand-rolled retry; left untouched (out of scope)."],
  "transcript_path": "C:\\Users\\<user>\\AppData\\Local\\llmdil-delegation\\transcripts\\2026-10-17\\141203-code_implementation-a3f9.jsonl"
}
```

Field definitions:

- **`status`** — exactly one of:
  - `succeeded` — completion declared AND the verification gate (§4.4)
    satisfied (code roles) or the evidence requirement satisfied (tool_task).
  - `partial` — the loop ended (cap or specialist judgment) with some
    criteria met and files changed. Distinct from failure because the work
    product may be worth keeping.
  - `blocked` — the specialist could not proceed and said so with evidence.
    No server malfunction implied.
  - `failed` — a malfunction or hard stop: gateway error, unparseable-output
    exhaustion, no-progress abort, fatal tool error. Something is wrong with
    the machinery or the model, not merely with the task.
- **`role` / `gateway_group`** — what actually ran. Group is included because
  role→group mapping is config (§7.4) and may change; the record should be
  true of this run.
- **`task_echo`** — the orchestrator's `task`, truncated to 300 chars, so
  Pete can correlate result ↔ delegation in a long session.
- **`summary`** — the specialist's final message, post-processed (§4.5.1).
  For `blocked`/`failed`, the first part is the server's own one-line stop
  reason, followed by the specialist's evidence where one exists. The summary
  must never claim success the verification block contradicts; when they
  disagree, the server rewrites the status and says so in the summary's first
  line. **The contract outranks the model's self-report.**
- **`acceptance`** — one entry per criterion: `met` true/false/null (null =
  not determinable), with a one-line `evidence` string. Server-assembled
  where mechanical (verification exit codes), specialist-stated otherwise.
  This is the field the orchestrator should reason over; `summary` is prose,
  this is data.
- **`files_changed`** — computed by the **server**, never reported by the
  model: pre-run git snapshot vs post-run state, or (non-git workspaces) the
  server's own log of successful write-tool calls. Line counts from
  `git diff --numstat` where available. Includes files changed before a
  failure — partial application is data, not an embarrassment to hide
  (§6.5).
- **`verification`** — last verification-relevant command, exit code, and a
  one-line result summary extracted by the server. `ran: false` is a
  meaningful, visible fact (and for code roles forces status ≠ `succeeded`).
- **`attempts`** — iterations, tool calls, wall-clock seconds, nudges,
  repairs. These numbers are how Pete — and the orchestrator — learn what a
  role actually costs; they are also the tuning input for §6.1 caps.
- **`blocked_reason`** — null unless `blocked`/`failed`: a machine-readable
  code (`scope_exceeded`, `gateway_unreachable`, `unparseable_tool_calls`,
  `no_progress`, `iteration_cap`, `wall_clock_cap`, `specialist_blocked`, …).
- **`notes`** — specialist observations outside the task scope (max 5, each
  ≤ 200 chars). Gives "I noticed…" a bounded home.
- **`transcript_path`** — absolute path to the full sub-loop log (§4.6). The
  pointer, not the payload.

For `tool_task`, `files_changed` is always `[]` (structurally guaranteed,
§5.2) and `verification` carries the primary command's exit code for shape
uniformity. Uniform shape across roles is deliberate: the orchestrator
learns one contract, not three.

#### 4.5.1 Size bound and enforcement

**Bound: the serialized result is ≤ 4,000 tokens (≈ 16 KB of text), with a
hard structural target of ≤ 8 KB for `succeeded` results.** The result lands
in the orchestrator's context and stays there for the rest of the Chatbox
session; ten delegations at the bound cost less than one pasted source file.

Enforcement, in order (all server-side, none trusting the model):

1. **Field-level caps at assembly.** `summary` ≤ 1,500 chars (truncation
   marked); each `evidence` ≤ 300 chars; `notes` ≤ 5 × 200 chars;
   `files_changed` ≤ 50 entries (beyond that: a count — a delegation
   changing >50 files is itself a finding). Tool outputs never enter the
   result except through these distilled fields.
2. **Summarization fallback.** If the assembled result still exceeds the
   bound, the server makes ONE extra gateway call to the role's own group
   with a purpose-built prompt ("Compress this delegation result JSON to
   under 3,000 tokens, preserving status, acceptance verdicts, file paths,
   and failure evidence verbatim") and uses the output if it parses as the
   same schema. A deliberate, bounded use of the model to police the model's
   output size.
3. **Hard truncation, last resort.** Truncate `summary` to 500 chars and
   drop `notes`. Structured fields (status, acceptance, files, verification,
   attempts) are never truncated away — if forced to choose, the contract
   keeps data and sacrifices prose.

Truncation is always marked, and cuts from the *middle* of the summary (head
+ tail preserved, elision marked): the last sentence is usually where the
caveats live, and a silently end-truncated summary would teach the
orchestrator to trust text that lost its most important sentence.

### 4.6 Sub-loop transcript: location, format, retention

The result contract is deliberately lossy. Learning — and debugging, and
auditing what a local model actually did to Pete's files — requires the full
record. Every sub-loop writes one transcript file.

**Where.** On the client machine, under the server's data directory:

```
%LOCALAPPDATA%\llmdil-delegation\
  config.json                     (§7.2)
  transcripts\
    2026-10-17\
      141203-code_implementation-a3f9.jsonl
      142251-tool_task-b71c.jsonl
  server.log                      (the server's own operational log)
```

Filename: `<HHMMSS>-<role>-<4-hex random>.jsonl` in a per-day directory. The
result's `transcript_path` points at the exact file — from any Chatbox
result Pete is one paste away from the full record. Transcripts live outside
any workspace: they concern many workspaces over time, and must not pollute a
project's git status.

**Format: JSONL (one JSON object per line), not markdown.** Its consumers
are (a) Pete inspecting with tools — `jq`, grep, editor JSON folding —
(b) possible future viewers/summarizers, and (c) the server itself, for
cross-run analysis (§6.3 tuning). All three want structured data; a readable
rendering can be generated from JSONL trivially, but the reverse conversion
is lossy. Each line is one event:

```json
{"ts":"2026-10-17T14:12:03.114-04:00","type":"run_start","role":"code_implementation","group":"coder","delegation_id":"a3f9","workspace":"D:\\dev\\sea-whisper-app","task":{},"pre_git_status":"…","caps":{}}
{"ts":"…","type":"request","iteration":1,"messages_count":2,"tools_offered":["read_file","edit_file"]}
{"ts":"…","type":"response","iteration":1,"finish_reason":"tool_calls","content":"I'll look at…","tool_calls":[{"name":"read_file","arguments":{"path":"src/net/fetch.rs"}}],"usage":{"prompt_tokens":1820,"completion_tokens":96}}
{"ts":"…","type":"tool_result","iteration":1,"tool":"read_file","ok":true,"duration_ms":12,"result_preview":"…first 500 chars…","result_full_hash":"sha256:…"}
{"ts":"…","type":"run_end","status":"succeeded","iterations":9,"wall_clock_s":214}
```

Two deliberate choices inside the format: tool results are stored as a
500-char preview plus a hash, not full text — full file reads and 32 KB test
outputs would balloon transcripts and duplicate the workspace; the hash lets
Pete verify "same content as on disk now" without storing it. Model requests
are not stored in full either — they are reconstructible (initial messages +
recorded responses/tool results, in order): the transcript *is* the recipe.
Per-response token `usage` is recorded verbatim from the gateway response;
it feeds §6.1 token accounting.

**Retention.** Configurable (`transcript_retention_days`); `0` = keep
forever. Expected size: a typical delegation transcript is 20–80 KB
(previews, not payloads) — 20 delegations/day for a year at keep-forever is
≈ 0.5 GB worst case, noticeable but not dangerous; at 30-day retention
≈ 45 MB. **Recommendation: ship with retention off (keep everything) while
Pete is learning and caps are being tuned** — the transcripts are the dataset
for those decisions, and deleting learning data to save disk Pete is not
short of would be the wrong default. Turn on 30-day pruning once behavior is
understood. Transcripts contain file-content previews and task text — same
sensitivity as the workspace itself; they stay under `%LOCALAPPDATA%`
(user-profile ACLs), are never in a workspace, and the server uploads
nothing anywhere. There is no telemetry in this design.

---

## 5. Role definitions

### 5.1 The three roles and their seats

| Role | MCP tool | Gateway group | Seat status (2026-10-17) | What the seat needs |
|---|---|---|---|---|
| Code implementation | `delegate_code_implementation` | `coder` | **Populated**: Qwen3-Coder-Next-80B MoE backend, enabled | Ready, subject to the §2.5 tool-call probe |
| Quick fix | `delegate_quick_fix` | `coder-fast` | Backend exists, **disabled / unpopulated** | A small, fast model with strong structured tool-call emission, placed for low latency (single GPU, no split — it should start fast and answer fast). Specific model selection is a catalog task, not settled here. Until populated, the role can be remapped to `coder` in server config (§7.4) to exercise the machinery. |
| Tool task | `delegate_tool_task` | `tool-runner` | Backend exists, **disabled / unpopulated** | A small model chosen for reliable structured output and instruction-following over raw coding power — its work is choosing commands and reporting numbers exactly. Same catalog-task caveat. |

A failed call against an unpopulated group must surface the gateway's own
error verbatim in the result `summary` (e.g. no healthy deployment for the
group) — a §6.5 gateway-path result, never a server crash (§6.5).

The `planner` group is not a delegation target — it is the seat the current
planner model occupies, and §8 recommends the orchestrator model that
replaces/augments it. `coder-nvidia` is unused in v1; a future role (e.g.
`delegate_code_review` on a second implementation model) arrives as a new
tool via config (§7.4) — that is the designed extension path.

### 5.2 Local tool sets per role

Roles differ in their local tools. The tool set is the specialist's
capability boundary — and since there is no approval prompt anywhere in this
system, capability boundary = safety boundary. A role that cannot write
files cannot damage files, no matter what its model hallucinates. Narrow
sets also help selection *inside* the sub-loop: a specialist offered 6
relevant tools picks more reliably than one offered 14 mixed tools.

**Shared read/inspection tools (all roles):**

| Tool | Arguments | Behavior / bounds |
|---|---|---|
| `list_dir` | `path`, `depth?` | Listing inside workspace; depth ≤ 3; ignores `.git/`, `target/`, `node_modules/` |
| `read_file` | `path`, `offset?`, `limit?` | UTF-8 text; default 400 lines, hard max 2,000; binary files refused with a clear error |
| `search_code` | `pattern`, `glob?`, `max_results?` | ripgrep-backed (fallback: recursive grep); ≤ 100 matches, each line truncated at 300 chars |
| `git_status` | — | `git status --porcelain` + current branch; read-only |
| `git_diff` | `path?` | Working-tree diff (optionally one path); truncated at 32 KB with marker |

**Execution tool (all roles, per-role allowlist scope):**

| Tool | Arguments | Behavior / bounds |
|---|---|---|
| `run_command` | `command`, `timeout_s?` | Allowlist by program family + argument pattern; output captured and truncated (§6.1); exit code always reported |

The allowlist is by *program family*, not arbitrary shell strings. v1
families: `cargo` (build/check/test/clippy/fmt --check/run with a package
scope) and the read-only git subcommands not already covered. **No shell
composition in v1** — no `&&`, `;`, `|`, redirection, or backticks; the
server splits and validates the command itself. This is stricter than a
human would tolerate, and that is the point: composition is where allowlists
go to die, and any genuinely needed pipeline can be added later as a named
composite in config, visibly.

**`code_implementation` — full set.** Shared tools + `run_command` (cargo
family) + write tools:

| Tool | Arguments | Behavior / bounds |
|---|---|---|
| `write_file` | `path`, `content` | Create/overwrite a whole file; content ≤ 256 KB |
| `edit_file` | `path`, `old_text`, `new_text` | Exact-match replacement, first occurrence; fails if `old_text` is absent or ambiguous, and the failure message says which, so the specialist can re-read and retry. Preferred over `write_file` for existing code: it fails safely on stale context instead of silently clobbering |
| `create_dir` | `path` | Inside workspace only |

**`quick_fix` — same verbs, tighter bounds.** Identical tool list (a fix may
need the same operations), different limits: `max_files_changed` (delegation
argument, default 2) is enforced by counting distinct paths passed to
`write_file`/`edit_file`; exceeding it stops the loop with
`blocked / scope_exceeded` (§6.4). Iteration and wall-clock caps are much
smaller (§6.1). The difference between the roles is therefore mostly budget
and prompt, not capability — which is correct: "quick" is a claim about
scope, and enforcement belongs on scope, not on which verbs exist.

**`tool_task` — read + execute, NO write.** Shared tools + `run_command`
(allowlist widened in config: cargo family plus read-only diagnostics Pete
configures). **No `write_file`, no `edit_file`, no `create_dir`.** The
argument, in full:

1. The role's purpose is *evidence* — run things and report what happened.
   Evidence that may have been produced by a role that can also edit the
   code under test is worthless as verification; an orchestrator delegating
   "check whether the tests pass" needs the answer to mean the tests *as
   they exist*.
2. It gives Pete and the orchestrator a genuinely safe delegation: the one
   role that can be fired speculatively, with zero risk to the tree — the
   natural tool for "go look" questions.
3. It creates a clean separation the orchestrator can exploit: implement
   with one role, verify with another, and the verifier's independence is
   structural, not prompted.

The predictable objection — "the tool task could fix a trivial problem it
finds" — is answered by the result contract: it reports the problem
precisely, and the orchestrator delegates a `quick_fix`. Two visible steps
beat one invisible fix.

One nuance: `run_command` can write indirectly (`cargo test` creates
`target/` artifacts; a test itself could write files). The boundary is
about *source* mutation through provided tools, stated plainly in the role
prompt. Cargo's outputs are excluded from `files_changed` accounting via
the git snapshot comparison with standard ignores.

### 5.3 Per-role system prompts (sketches)

System prompts are server-side constants (files next to config, §7.4; each
transcript records the prompt's SHA-256 so a behavior change is attributable
to a prompt change). Sketches below are the key instructions, not full text;
each runs ~200–350 words in production — long enough to be specific, short
enough that a local model actually weights all of it.

**Shared preamble (all roles, server-prepended):**

> You are a specialist agent working inside an automated sub-loop. You never
> talk to the user. You work only through the tools provided. All file paths
> are relative to the workspace root given below. State what you are about
> to do in one short sentence before tool calls. When you have met the
> acceptance criteria, verify as instructed, then reply with a final summary
> and no tool calls. If you cannot complete the task, say so plainly in your
> final reply, starting with the word BLOCKED, followed by what you tried
> and the exact evidence of failure. Never claim a test passed unless you
> ran it and saw it pass in this session.

The last two sentences are the contract §4.4 and §6.4 mechanically depend
on: completion and blockage must be *detectable in the final message*, not
inferred from vibes.

**`code_implementation`:**

> Plan before editing: read the relevant code first and form a complete
> picture of the change. Prefer editing existing files with `edit_file` over
> rewriting whole files. Keep the change scoped to the task; do not refactor
> unrelated code, rename public APIs, or add dependencies unless the task
> requires it — if it does, say so in your summary. After each significant
> edit, build (`cargo check` for speed) and fix errors before moving on.
> Before finishing, run the verification command from your task (or the test
> suite for affected crates) and report its result exactly. Match the
> codebase's existing style and error-handling patterns.

**`quick_fix`:**

> This is a small, scoped fix. Find the locus, make the smallest change that
> resolves it, and verify. You may change at most {max_files_changed}
> file(s); if the real fix needs more, stop and reply BLOCKED with your
> evidence — do not expand scope. Do not refactor, do not reformat unrelated
> code, do not fix other problems you notice (note them in your summary
> instead). Speed matters more than elegance, but a fix that breaks another
> test is not a fix: run the affected tests before finishing.

**`tool_task`:**

> Your job is to run commands and inspect the workspace, and return evidence
> that answers the task's question. You have no file-writing tools and must
> not attempt to modify source files. Prefer precise commands over broad
> ones (`cargo test -p <crate>` over the whole workspace) unless the task
> asks for breadth. Report numbers exactly as observed (tests passed/failed
> counts, exit codes, timings) — never round, never summarize a failure as
> 'some errors'. Quote the key lines of any failure output verbatim in your
> final summary. If a command is not available or not permitted, report that
> as the finding rather than working around it.

---

## 6. Control and safety

Because there is no human approval prompt (§1), these controls are the
entire governance of the sub-loop. They are set per role, enforced by the
server, and reported in every result (`attempts`).

### 6.1 Per-role caps

v1 starting values — generous enough that a competent specialist finishes
real work, tight enough that a looping model wastes minutes, not hours.
They live in config (§7.4); §4.6 transcripts are the evidence base for
retuning them.

| Cap | code_implementation | quick_fix | tool_task | Enforcement |
|---|---|---|---|---|
| Iterations (model calls) | 25 | 10 | 15 | Counter per gateway request; at cap the loop ends: `partial` (criteria partly met + files changed) or `failed/blocked_reason: iteration_cap` |
| Wall-clock | 20 min | 5 min | 10 min | Monotonic clock from run start; checked before each model call and tool execution |
| Response tokens per call | `max_tokens: 4096` | 2048 | 2048 | Sent on every request; prevents one rambling response from eating the budget |
| Cumulative completion tokens | 60,000 | 15,000 | 25,000 | Server sums `usage.completion_tokens` per gateway response; if a backend returns no `usage`, the server estimates (chars/4) and marks the transcript `usage_estimated: true` — accounting is never silently absent |
| Per-command timeout | 300 s (verification commands; default tool timeout 60 s) | 180 s | 120 s | Tool-level; a timeout is a tool *error result* (§6.5), not immediately fatal |
| Single tool-result size into context | 32 KB | 32 KB | 32 KB | Longer outputs truncated head+tail with an explicit marker and byte counts; full output is written to the transcript directory as a sidecar file referenced by hash — "truncated" never means "the evidence is gone" |
| Total sub-loop context guard | If serialized history exceeds ~70% of the backend's known context window (per-group figures come from LLMdil catalog/config, not a second source invented here), the server compacts: older tool results are replaced with one-line digests (tool, ok/err, first line). Still >90% → end as `partial` rather than let the backend truncate silently mid-thought | same | same | Server-side estimate (chars/4 per message); approximate by design, conservative thresholds |

Why three different budget kinds: each catches a failure the others miss. A
loop can burn 25 iterations in 90 seconds (cheap calls, no progress —
iterations catch it), sit inside one 19-minute `cargo test` (wall-clock
catches it), or produce enormous reasoning outputs in few calls (tokens
catch it). Any single budget is gameable by accident; models don't game
deliberately, they just fail in varied shapes.

### 6.2 What happens at a cap

Caps are not crashes. When a cap fires, the server still computes
`files_changed` from git state, assembles `acceptance` from evidence so far,
sets `summary`'s first line to its own statement ("Stopped: iteration cap
(25) reached."), and returns normally. The specialist gets no "one last
call" at a hard cap — a model that needed 25 iterations does not reliably
finish in one, and the transcript preserves exactly where it stopped if Pete
wants to re-delegate with a narrower spec.

### 6.3 No-progress detection

Iteration caps bound waste; no-progress detection cuts it earlier and, more
importantly, *classifies* it — "looping" is actionable for the orchestrator
("re-specify or take over") in a way "ran out of iterations" is not. All
signals are computed server-side per iteration:

1. **Identical tool call repetition.** Same tool + semantically identical
   arguments (after JSON canonicalization) as a previous call in this run,
   with no intervening file write. The same call 3 times → abort
   `failed / no_progress`. (Re-reads after an edit are excluded by the
   no-intervening-write clause — re-reading a file you just changed is
   normal.)
2. **Ping-pong.** Alternating A,B,A,B between two tool calls for 6
   consecutive calls → same abort.
3. **Repeated identical failure.** The same tool returning an identical
   error 3 times consecutively (e.g. an `edit_file` match failure on the
   same text) → abort `blocked / no_progress` — blocked rather than failed,
   because the evidence ("this exact edit cannot apply") is a task-level
   fact the orchestrator can use.
4. **Error streak.** 5 consecutive tool results that are all errors → abort
   `blocked / no_progress` with the errors quoted.

On abort, the result's `summary` names the pattern in plain language ("The
specialist called read_file on src/net/fetch.rs 3 times without intervening
changes and made no edits"). For Pete reading the Chatbox transcript, this
line teaches more about local-model failure modes than any other field in
the contract.

Not in v1: semantic progress scoring (asking a model "is this progress?").
It costs calls, it is unreliable with the same models whose behavior it
judges, and the mechanical signals above cover the observed failure shapes.
Revisit only with transcript evidence that mechanical detection is missing
real stalls.

### 6.4 Escalation semantics

Escalation is the sub-loop's designed way to say "a decision is needed that
I shouldn't make." Four producers:

- **Specialist-declared:** final message beginning `BLOCKED` (§5.3) →
  `blocked / specialist_blocked`, evidence = its message.
- **Scope guard:** quick_fix exceeding `max_files_changed`; any role
  attempting a write outside its workspace scope (the attempt is refused;
  attempting twice escalates rather than merely erroring) →
  `blocked / scope_exceeded`.
- **Mechanical:** no-progress (§6.3); missing prerequisites (spec references
  a path that doesn't exist and search finds nothing similar; verification
  command not allowlisted) → `blocked` with the specific code.
- **Verification impasse:** after 2 nudges (§4.4) the specialist still
  cannot produce a passing verification → `blocked / verification_failed`,
  with the last failing output's key lines as evidence.

What `blocked` means for the orchestrator (also stated in each delegation
tool's description — this is the outer loop's half of the contract): a
`blocked` result is information, not an instruction to retry blindly.
Intended responses, in order of preference: (1) answer the block from the
orchestrator's own knowledge and re-delegate with a corrected spec; (2)
delegate a `tool_task` to gather the missing fact; (3) narrow the task and
retry **once**; (4) do the work in the outer loop; (5) report the block to
Pete with the evidence.

The server enforces a weak form of this discipline honestly: it cannot
track identical-spec re-delegation across calls (it is stateless between
delegations within a session), so anti-retry discipline stays a prompt-level
norm for the orchestrator. What the server *does* enforce: **it never
retries internally on its own initiative. One `tools/call` = one sub-loop =
one result.** Retry policy belongs to the actor with the context to retry
*differently* — the orchestrator.

`failed` (vs `blocked`) carries the opposite implication: don't re-delegate
the same way; the machinery or model misbehaved (gateway down, unparseable
output, cap exhaustion mid-edit). The orchestrator's sensible moves are
report, wait/retry later (gateway causes), or escalate to Pete. The
blocked/failed distinction exists precisely so the orchestrator doesn't need
to parse prose to choose between "fix the spec" and "fix the system."

### 6.5 Failure modes, one by one

**Gateway unreachable** (Prometheus off, gateway stopped, LAN issue, wrong
base URL). Connection retries with backoff — 3 attempts (1 s / 3 s / 9 s) —
for the first model call only. A gateway that disappears *mid-loop* gets one
retry per call (2 s), then the run ends `failed / gateway_unreachable`: a
flapping backend corrupts timing assumptions for everything downstream. No
queuing, no background retry — the orchestrator decides. The result names
the base URL used and the OS error class (connection refused vs timeout vs
DNS); the three have different fixes and Pete will be the one fixing them.
A cheap pre-flight helps the common case: at server startup (and lazily on
the first delegation after >5 min idle) the server GETs `{base}/models` with
the master key; failure is logged to `server.log` so Pete learns the gateway
is down when Chatbox starts, not mid-task. Auth corollary: a 401 (wrong or
rotated master key) is reported distinctly as `gateway_auth_failed`, never
lumped into "unreachable" — it will not fix itself on retry.

**Specialist emits unparseable tool calls.** Shapes observed with local
llama.cpp models: (a) valid `tool_calls` structure but the `arguments`
string is malformed JSON; (b) the tool call rendered as text in `content`
(e.g. `<tool_call>…</tool_call>` markup leaking — the exact outcome the MCP
capabilities design's probe records as "unparsed"); (c) a call to a tool
that doesn't exist in this role's set; (d) arguments failing schema
validation.

- (a) and (d): the server appends a synthetic `tool`-role error for that
  call id — "Your arguments for `edit_file` were not valid JSON / failed
  validation: <specific error>. Re-emit the call with corrected arguments."
  This counts as a **repair**, and it consumes an iteration (otherwise a
  broken model loops repairs forever). Repair budget: **3 per run**; a 4th
  parse failure ends the run `failed / unparseable_tool_calls`, with the raw
  offending output preserved in the transcript.
- (b): the server does **NOT** scrape text for tool calls. Parsing model
  prose into executable calls would defeat the structured-calling
  precondition the Phase 0 probes exist to verify, and would hide a
  backend/template problem Pete needs to know about. The response is treated
  as a final text answer — which, containing no verification, hits the §4.4
  gate path and ends `blocked`/`failed` honestly. The fix for repeated (b)
  lives at the backend (chat template / `--jinja` / handler), and the
  transcript's recorded `finish_reason` + raw content is the evidence.
- (c): a `tool` error listing the tools that DO exist in this role. Models
  self-correct this reliably; it consumes an iteration but not repair budget
  (well-formed, just wrong — a reasoning slip, not a format failure).

**Sub-loop fails midway, after files were already written.** There is no
transactional filesystem, and this design does not fake one (no shadow
copies, no write staging — staged writes would break `cargo test` realism;
the specialist must test the actual tree). Instead, partial application is
made **visible and recoverable**:

- `files_changed` is computed from git state at *every* exit, including
  failures — a `failed` result still lists exactly what changed, with
  numstat.
- Any non-`succeeded` run that changed files includes in `summary` the
  server's line: "Working tree differs from pre-run state (N files). Pre-run
  HEAD: `<sha>`. Review with `git diff`; revert with `git checkout --
  <paths>`." Instructions, not just data, because the reader may be the
  orchestrator deciding what to tell Pete.
- The pre-run snapshot records whether the tree was already dirty
  (`tree_was_dirty: true` in the result when applicable), so a post-failure
  diff is never misattributed to the specialist.
- Checkpoint norm (stated in tool descriptions): before delegating
  implementation work on a dirty tree, note it — and Pete's habit to build:
  commit or stash before a delegation session.
- Optional, v1 if cheap, v1.1 otherwise: on run start in a git workspace,
  create a lightweight safety ref `refs/llmdil-delegation/<delegation_id>`
  pointing at HEAD (no checkout, no working-tree effect). Rollback then
  means diffing against a fixed point even if Pete commits mid-run.
  Bookkeeping, not transactionality, and described as such.

**Tool execution errors: fed back vs fatal.** The dividing rule: *an error
is fed back as a `tool` message if a competent agent could plausibly recover
with a different next action; it is fatal if continuing would be meaningless
or unsafe.*

- Fed back (recoverable): file not found, `edit_file` match failure, search
  with no matches, command non-zero exit (test failures are the normal
  texture of the loop — exit code + output return as the tool result with
  `ok: false` and the specialist iterates), command timeout (partial output
  + "timed out after Ns"), argument validation errors.
- Fatal to the run: workspace-scope violation on write (§7.3),
  server-internal errors (disk full writing a transcript, invalid config),
  gateway failures (above), cap/no-progress aborts (§6.1–6.3).
- Middle case, treated explicitly: the same command failing with the same
  error repeatedly is fed back each time, but §6.3's repeated-failure
  detector converts it into a `blocked` escalation — recoverable per
  instance, escalated in aggregate. That two-level treatment is the general
  pattern: **tools report; the loop governor decides.**

**Orchestrator-side failure:** Chatbox killed mid-delegation orphans the
server (stdio EOF). The server treats stdin close as an abort signal, kills
any in-flight command process tree, marks the transcript
`run_end: aborted_host_closed`, and exits. A half-edited tree in that case
is covered by the same git-visibility story; the transcript's last complete
events show exactly where work stopped.

---

## 7. Implementation shape

### 7.1 Language/runtime for a stdio MCP server on Windows

The server is small in concept — JSON-RPC plumbing, an HTTP client, a loop
governor, ~10 tool implementations, process execution with timeouts — but it
is *load-bearing*: every safety property in §6 and §7.3 lives in this code.

**(a) Python + official MCP SDK (FastMCP).**
- *Shape:* a small package — `server.py` (transport/tool registration),
  `loop.py` (the sub-loop and governor), `tools.py` (local tool
  implementations), `roles.py` (role config) — the 3–4 file split is itself
  instructive. FastMCP decorates functions into MCP tools; the SDK handles
  stdio transport, `tools/list`/`tools/call`, and schema generation.
- *Hackability:* highest of the three for this user. Pete reads Python
  fluently; the gateway stack he already operates (LiteLLM) is Python; the
  sub-loop's HTTP calls are plain `httpx` against an OpenAI-shaped API — no
  framework magic between Pete and the wire. When a delegation misbehaves,
  the debugging surface is a `.py` file and a JSONL transcript, both
  greppable. No build step: "edit the system prompt, restart Chatbox" is the
  whole iteration cycle — exactly right for tuning role prompts while
  learning.
- *Startup:* the weak point, manageable. CPython cold start + SDK import is
  ~300–800 ms (ESTIMATE) — imperceptible against multi-second model calls.
  The real cost is environment: a venv must exist and the registration must
  point at its interpreter (§7.5).
- *Packaging:* a project folder + venv + a one-page `setup.ps1`
  (`py -m venv`, `pip install mcp httpx pydantic`, pinned `requirements.txt`).
  Not a single binary. Distribution is a non-goal (one user, one machine);
  reproducibility is the goal, and pinning achieves it.
- *Maturity:* the official Python SDK is the reference MCP implementation
  lineage; FastMCP is its high-level layer and the most widely used Python
  MCP framework. Risk: FastMCP's convenience can hide the protocol —
  mitigated by keeping the loop code explicit and using FastMCP only for
  transport/tool registration, which is also the honest way to learn where
  MCP ends and the agent begins.
- *Windows specifics:* stdio + Python is well-trodden; `PYTHONUTF8=1` /
  `PYTHONIOENCODING=utf-8` must be set in the spawn env (§7.5) so JSON-RPC
  text isn't mangled by a legacy codepage. Process-tree kill on
  timeout/abort uses `taskkill /T` or Job Objects — a solved problem,
  ~20 lines.

**(b) Rust binary (`rmcp` SDK).**
- Single `llmdil-delegation.exe`, ~10 ms start (ESTIMATE), no interpreter, no
  venv, trivially copied — best in class on startup and packaging.
- Hackability is the problem *for this artifact*: not Pete's Rust ability
  (LLMdil is a Rust codebase) but the learning target. The subject here is
  agentic architecture, and Rust's borrow/async friction taxes exactly the
  parts Pete will experiment with weekly — loop-governor tweaks, schema
  fiddling, ad-hoc transcript analysis all fight the compiler's cadence.
  Every experiment costs a build.
- `rmcp` is the official Rust SDK and actively developed, but its example
  ecosystem is a fraction of Python's. Also deliberate: the delegation server
  stays *out* of the LLMdil Rust workspace — it is a client-machine artifact
  with a different release rhythm.

**(c) TypeScript MCP SDK (`@modelcontextprotocol/sdk`).**
- Middle hackability: readable, and the SDK is the most battle-tested of the
  three (the protocol's home ecosystem; the largest server corpus to crib
  from). JSON handling is native. Costs: a Node toolchain dependency for one
  small server on a machine whose other AI tooling is Python/Rust; Node cold
  start ~150–400 ms (ESTIMATE); packaging is `node_modules` or a bundled
  single file — workable, not clean. (The classic Node stdio lesson: stdout
  IS the protocol channel — one stray `console.log` corrupts JSON-RPC.)

**Recommendation: (a) Python + FastMCP for the MVP.** The deciding criterion
is the project's purpose: this server exists so Pete can learn agentic
architecture by running and modifying one. Python minimizes the distance
between "I wonder what happens if the governor treats X differently" and
finding out. Startup and packaging disadvantages are real but irrelevant at
one user on a machine that already runs Python tooling. TypeScript is the
credible runner-up (best ecosystem); Rust is the right *eventual* answer if
this graduates from learning project to daily driver.

> **DECISION RECORDED — 2026-10-17, Pete: implement in Rust from the start.**
> Pete overruled the recommendation above the same day the design was
> accepted, under his standing rule: write everything in Rust that can
> reasonably be written in Rust. The server is built with **(b) Rust +
> `rmcp`** from the first commit — taking the "graduation path" immediately
> rather than porting later. His reasons beyond the rule itself: a single
> self-contained `.exe` with no interpreter/venv to manage on the dev
> machine, and one language across LLMdil and its companion tooling. The
> tradeoff is stated plainly: the edit-inspect iteration loop while
> learning is slower in Rust — every governor or schema experiment costs a
> compile — and that cost is accepted deliberately. Where this document
> says "Python + FastMCP project" elsewhere (notably §10 Phase 1), read it
> as superseded by this decision; the module split in (a) carries over
> (transport/registration, loop governor, local tools, role config) as Rust
> modules. Everything else in this design — wire contracts (§3.3, §4.5),
> caps (§6.1), containment (§7.3), transcripts (§4.6) — is
> language-neutral and unchanged.

**What would justify switching:**
- To Rust: delegation becomes a daily, latency-sensitive tool; or
  distribution beyond Pete's machine is wanted; or role/prompt behavior has
  been stable for months (nothing left to learn by tinkering — freeze it in
  a compiled artifact). A port is cheap by design: the wire contracts (§3.3,
  §4.5) and JSONL format (§4.6) are language-neutral, and the Python
  implementation becomes the executable spec.
- To TypeScript: repeated MCP spec-edge interop problems traceable to the
  Python SDK, or Pete standardizes his MCP servers on the TS ecosystem.
- Explicitly NOT a switch trigger: raw loop throughput. The loop is
  bottlenecked by model inference on another machine by two orders of
  magnitude; the server's own CPU use is noise.

### 7.2 Configuration: gateway, key, workspace

One config file, one environment override layer, no hardcoded secrets —
ever. **Location:** `%LOCALAPPDATA%\llmdil-delegation\config.json` (same
data directory as transcripts, §4.6). Versioned by `"version": 1`. Unknown
fields are rejected at load — fail loudly, the same philosophy as §3.3
applied to operations.

```json
{
  "version": 1,
  "default_deployment": "prometheus",
  "deployments": {
    "prometheus": {
      "base_url": "http://192.168.5.23:4000/v1",
      "master_key_env": "LLMDIL_GATEWAY_KEY"
    }
  },
  "workspace_root": "D:\\dev",
  "transcript_retention_days": 0,
  "roles": { "…": "see §7.4" }
}
```

**Master key handling.** The key is NEVER in `config.json`, never in source,
and never in the server repo. Sources, in precedence order:

1. The environment variable named by the deployment's `master_key_env`
   (default `LLMDIL_GATEWAY_KEY` for `prometheus`), set in the Chatbox MCP server env block (§7.5) —
   the v1 primary path. Chatbox passes env to the stdio child; Chatbox
   stores its config locally under Pete's profile; the key is a LAN-scoped
   master key for his own appliance. Chosen with eyes open.
2. Fallback: Windows Credential Manager target
   `llmdil-delegation/gateway-master-key` (the same store pattern LLMdil
   itself uses for MCP secrets in the capabilities design), read at startup
   if the env var is absent.

The server logs only "key source: env | credential-manager | MISSING" at
startup. If missing, the delegation tools still register (so Chatbox shows
them) but every call returns `failed / gateway_auth_failed` with remediation
text — a discoverable failure, not a silent absence. Key *values* never
appear in transcripts, results, or logs; the transcript header records the
base URL and key source only. Rotation: Pete changes the value in Chatbox
env / Credential Manager and restarts Chatbox; the server holds the key in
memory only, for its process lifetime.

**Base URL.** Default is the LAN address. If Pete runs Chatbox on the
Prometheus host itself (plausible for testing, not the directed setup), he
adds a second deployment pointing at `http://127.0.0.1:4000/v1` — one
config block. No auto-discovery, no fallback probing between the two:
silently succeeding against a different gateway than configured would be a
debugging nightmare disguised as robustness.

**DECISION RECORDED — 2026-10-17 (deployments, Pete).** The gateway target
is a *named deployment*, resolved entirely server-side: config carries a
`deployments` map (name → `base_url` + key env var) with a
`default_deployment`, and each delegation tool accepts one optional
`deployment` argument carrying a NAME only (§3.3). An unknown name is a
clean error listing the configured names — no fallback, and no HTTP
request is made. The effective deployment name and base URL are recorded
in every transcript header (§4.6) and every result (§4.5). The rejected
alternative — a raw gateway URL accepted per call — was dropped for two
reasons: the URL is a constant the model would have to reproduce verbatim
in every call (a transcription-error surface with no benefit), and a
per-call URL turns the server into a master-key exfiltration vector —
anything that influences the orchestrator could aim the server's key at an
attacker-chosen endpoint. Keys stay server-side; names are the only thing
a caller supplies.

### 7.3 Workspace root: configuration and enforcement

- `workspace_root` in config is the default scope; a delegation's
  `workspace_subdir` narrows it per call. All local tools (§5.2) operate
  relative to the effective scope root.
- **Canonicalization, on every tool call** (not just at run start — a
  specialist assembles paths one call at a time): resolve the requested
  path against the scope root, canonicalize (absolute, `.`/`..` resolved,
  symlinks/junctions resolved via `Path.resolve()`), then assert the result
  is the scope root or strictly beneath it (`is_relative_to`). Failure is a
  tool error for read attempts, a **fatal scope error for write attempts**
  (§6.4). String-prefix checks without canonicalization are explicitly not
  acceptable — `D:\dev\..\secrets` passes a naive prefix test.
- **Junctions/symlinks inside the workspace pointing outside it:**
  canonicalization resolves them to their targets, so they fail containment
  — correct behavior even when Pete created the junction deliberately. The
  workaround is widening `workspace_root` in config, a visible act, rather
  than the server trusting links.
- **Case:** Windows paths are case-insensitive; containment comparison uses
  case-folded canonical paths (`os.path.normcase`). Skipping this is a
  classic Windows-only escape (`D:\DEV\…` vs `D:\dev\…`).
- **Reserved denials regardless of scope:** writes to `.git\` internals (git
  history changes go through git commands, and v1's git tools are
  read-only), and any path under the server's own data directory — a
  specialist must not be able to edit the transcripts or config that govern
  it.
- `run_command` executes with `cwd` = effective scope root and a **scrubbed
  environment: the gateway key variable is removed** from child processes'
  env. A build script has no business reading the key, and "the model asked
  cargo to print the environment" should be a boring transcript event, not
  a leak.

### 7.4 Role → group mapping and per-role config

Roles are data, not code. The server's config carries the full role table —
which is also where §3.2's redeploy cost lives (edit JSON, restart server):

```json
"roles": {
  "code_implementation": {
    "tool_name": "delegate_code_implementation",
    "gateway_group": "coder",
    "system_prompt_file": "prompts/code_implementation.md",
    "caps": { "max_iterations": 25, "wall_clock_s": 1200, "max_completion_tokens_total": 60000, "response_max_tokens": 4096 },
    "local_tools": ["list_dir","read_file","search_code","git_status","git_diff","write_file","edit_file","create_dir","run_command"],
    "command_allowlist": ["cargo"]
  },
  "quick_fix": {
    "tool_name": "delegate_quick_fix",
    "gateway_group": "coder-fast",
    "system_prompt_file": "prompts/quick_fix.md",
    "caps": { "max_iterations": 10, "wall_clock_s": 300, "max_completion_tokens_total": 15000, "response_max_tokens": 2048 },
    "local_tools": ["list_dir","read_file","search_code","git_status","git_diff","write_file","edit_file","run_command"],
    "command_allowlist": ["cargo"]
  },
  "tool_task": {
    "tool_name": "delegate_tool_task",
    "gateway_group": "tool-runner",
    "system_prompt_file": "prompts/tool_task.md",
    "caps": { "max_iterations": 15, "wall_clock_s": 600, "max_completion_tokens_total": 25000, "response_max_tokens": 2048 },
    "local_tools": ["list_dir","read_file","search_code","git_status","git_diff","run_command"],
    "command_allowlist": ["cargo"]
  }
}
```

Notes:

- System prompts live as files next to config (`prompts\*.md`) so Pete can
  iterate on the highest-leverage text in the system without touching code.
  Each transcript header records the prompt file's SHA-256.
- Because `coder-fast` and `tool-runner` are unpopulated (§5.1), the v1
  bring-up order sequences `coder` validation first (§10), and mapping
  `quick_fix` → `coder` temporarily **in config** is a designed use of this
  table, not a hack.
- The orchestrator's own model/group is Chatbox provider configuration
  (custom OpenAI provider → gateway base URL, the orchestrator group,
  tool-capable flag on — per the verified Chatbox facts), entirely outside
  this server. Stated here only to keep the boundary crisp.

### 7.5 Chatbox registration (Settings → MCP)

Assumes the server project lives at `C:\Users\<user>\llmdil-delegation\`
with a venv at `.\.venv\` (created by `setup.ps1`: `py -3.12 -m venv .venv`,
then `.\.venv\Scripts\pip install -r requirements.txt`).

```json
{
  "mcpServers": {
    "llmdil-delegation": {
      "command": "C:\\Users\\<user>\\llmdil-delegation\\.venv\\Scripts\\python.exe",
      "args": ["C:\\Users\\<user>\\llmdil-delegation\\server.py"],
      "env": {
        "LLMDIL_GATEWAY_KEY": "<gateway master key>",
        "PYTHONUTF8": "1",
        "PYTHONIOENCODING": "utf-8"
      }
    }
  }
}
```

Details that matter:

- **Absolute paths everywhere.** Chatbox spawns with an unpredictable
  working directory; relative paths in `command`/`args` are the #1 cause of
  "server won't start" in stdio setups. Pointing `command` at the venv's
  `python.exe` (not system `py`, not a `.bat` shim) removes PATH and
  activation ambiguity entirely.
- The key appears in Chatbox's settings under this option — that is §7.2's
  primary path, chosen deliberately. What it must never appear in: the
  server repo, `config.json`, or git anywhere.
- `PYTHONUTF8` / `PYTHONIOENCODING` per §7.1 — stdio JSON-RPC carrying
  non-ASCII file contents (or a stray — in a task string) must not corrupt
  on a legacy codepage.
- Server name `llmdil-delegation` becomes the namespace prefix Pete sees in
  Chatbox's tool list; keep it stable — renaming the server re-namespaces
  every transcript reference.
- Install smoke test: after saving, Chatbox restarts the server and calls
  `tools/list`; the three `delegate_*` tools should appear, and `server.log`
  should show the gateway pre-flight result (§6.5). That, plus those two
  lines, is the whole validation before §9's worked example can be run live.

---

## 8. Orchestrator model

Full research evidence: `~/workspace/.tmp-assets/delegation-design/model-research.md`.
This section carries the verdicts, the numbers they rest on, and the backend
configuration sketches. Labels per the convention at the top of this
document.

### 8.1 What the seat demands

The orchestrator plans, decomposes tasks into task specs, emits delegation
tool calls, and interprets result contracts. Its binding constraints on
Prometheus (Threadripper PRO 5945WX, 96 GB RAM now / 128 GB possible,
RTX A4000 16 GB + one Tesla P100 16 GB visible):

- **It must coexist with the coder backend.** The realistic steady state has
  both loaded. The coder (Qwen3-Coder-Next-80B, UD-Q4_K_M, 49,301,055,488 B
  file) holds ~43.5 GiB of routed experts in system RAM under
  `--n-cpu-moe 46`. Any orchestrator sizing that only works on an empty
  machine is wrong.
- **Routed experts are 96–98% of every candidate's parameters** (DERIVED), so
  under full expert offload a model's system-RAM cost ≈ its file size. There
  is no quant trick that makes a very large model cheap in RAM.
- **The P100 is not assumed.** It had a thermal incident (81 °C idle, fell
  off the bus, recovered). Sustained heavy load on it is out until Pete
  confirms sane idle temperatures after refitting the case. Recommended
  steady-state configurations put no expert compute on the P100 at all.

### 8.2 Recommendation: Qwen3-Next-80B-A3B-Thinking (primary)

**File (VERIFIED, Hugging Face API):**
`unsloth/Qwen3-Next-80B-A3B-Thinking-GGUF` /
`Qwen3-Next-80B-A3B-Thinking-UD-Q3_K_XL.gguf` — single file,
**35,494,473,888 bytes** (33.06 GiB),
SHA-256 `b476f67269f0698b76832d0190b56dd6f9027795c3600d3a424a0ca48f6b51c0`
(obtained two independent ways from the HF API — `?blobs=true` siblings LFS
metadata and the tree endpoint's `lfs.oid` — agreeing exactly; re-verify
locally after download, per house convention).

**Architecture (VERIFIED, official `config.json` + card):** 80B total / 3B
active; 48 layers in a hybrid layout — only 12 of 48 layers are
full-attention (Gated DeltaNet layers keep a fixed-size recurrent state
instead of a growing KV cache); 512 experts, 10 routed per token + 1 shared;
native context 262,144.

**Why this one:**

- **A clear reasoning step up from the incumbent 27B dense planner.** Vendor
  card (VERIFIED as vendor claims): AIME25 87.8, GPQA 77.2, HMMT25 73.9 —
  above Qwen3-32B-Thinking on every reasoning row of its own card, and the
  27B dense planner sits below that 32B class. Agent scores are at flagship
  level: BFCL-v3 72.0, level with Qwen3-235B-A22B-Thinking's 71.9. No vendor
  table benchmarks it directly against the 27B planner, so no head-to-head
  number is claimed — the step-up argument is the honest form above.
- **It is the only candidate that fits the concurrent case at 96 GB** (§8.4).
- **Long context is nearly free in KV terms** (DERIVED from VERIFIED
  config): 24,576 B/token of KV, so the recommended 65,536 context costs
  ~0.75 GiB at q8_0 — versus ~6 GiB for a full-attention model of this class.
  Multi-step delegation histories fit without squeezing the A4000.
- **llama.cpp support is first-class** (VERIFIED): the `qwen3next`
  architecture has a dedicated `GATED_DELTA_NET` op (CPU + CUDA, PR #19504)
  and ongoing graph optimization in llama.cpp. Unsloth's UD dynamic quants
  keep sensitive tensors at higher precision than the nominal bitrate — the
  standard preference for a model whose job is structured output (that
  preference is judgment, not a vendor claim).

**Honest caveats:**

- **Structured tool-call parsing must be smoke-tested, not assumed.**
  llama.cpp's `docs/function-calling.md` names native handlers for Qwen 2.5
  (Hermes-style) and Qwen3-Coder XML, with a generic fallback for
  unrecognized templates; no handler names Qwen3-Next specifically
  (VERIFIED from the doc). The GGUF carries the model's Jinja template with
  the Qwen3-family `<tool_call>` format; whether llama-server returns parsed
  `tool_calls` with `--reasoning-format deepseek` keeping the think trace in
  `reasoning_content` is Phase-0 test item #1 for this model (§8.5). This is
  the same probe class as §2.5, applied to the orchestrator seat.
- **Thinking mode is forced.** The card states the Thinking checkpoint
  "supports only thinking mode" and may generate long traces; every
  delegation decision pays a think trace first. Mitigations are in the
  config sketch (per-request output cap, §8.6). The escape hatch is the
  Instruct variant of the same family — `Qwen3-Next-80B-A3B-Instruct-UD-Q3_K_XL.gguf`,
  35,637,611,296 B (VERIFIED), BFCL-v3 70.3, no think latency — a config
  change, not a redesign, if Phase-0 testing shows forced thinking hurting
  delegation latency or tool emission.

**Speed: generation 12–25 t/s, prompt processing 150–400 t/s — ESTIMATE.**
Reasoning: ~0.63 GB/token of active expert bytes streamed from system RAM
(DERIVED from the VERIFIED file size and expert share) plus ~6 GFLOP/token
of active compute, on 12 physical cores with 8-channel DDR4 — CPU-bound well
below raw bandwidth. Anchors (VERIFIED as reported, different regimes): a
laptop Core Ultra 9 185H ran this model in llama.cpp at 2.8 t/s generation
(treated as a weak-hardware floor, not an expectation); the sibling
Qwen3-Coder-Next-80B measured 7.59 t/s CPU-only at BF16 (the heaviest
possible regime) on a 16-core EPYC. Even the estimate's bottom end emits a
500-token plan + tool call in ~40 s; the top is comfortable interactive use.
Measure in Phase 0 (§8.5); below ~8 t/s, switch to the Instruct variant
before blaming the hardware.

### 8.3 Fallback: GPT-OSS-120B

**File (VERIFIED):** `ggml-org/gpt-oss-120b-GGUF` / `gpt-oss-120b-MXFP4.gguf`
— **63,387,346,208 bytes** (59.03 GiB),
SHA-256 `582bd40f6886200101f4c4ed9f25f3fe80cc14c86e9e2b37746cd8904a0c622d`
(same two HF API paths, agreeing).

**Architecture (VERIFIED):** 116.8B total / 5.1B active; 36 layers; 128
experts, 4 per token; native context 131,072. Experts are natively MXFP4 —
98.2% of parameters are routed experts (DERIVED), so there is almost no
non-expert weight to hide on a GPU.

**Why it is only the fallback, despite the best tool-calling story in the
set:**

- **Best tool-calling support, with an integration tax.** llama.cpp names a
  dedicated **"GPT-OSS (Harmony)"** native tool-call handler (VERIFIED) —
  the strongest parser story of any candidate. But harmony is OpenAI's own
  response format (analysis/commentary/final channels), not ChatML and not
  Qwen XML: the client/gateway path must speak it end-to-end or tool calls
  silently degrade. Under llama.cpp's native handler this is manageable;
  through the Chatbox/LiteLLM path it needs verification (§8.5, item 5).
- **It cannot shrink.** Requantizing does not help — every published variant
  is 58–60 GiB, because the experts are already MXFP4 (VERIFIED from the
  size listings themselves). There is no 35 GiB GPT-OSS-120B.
- **It does not fit concurrently at 96 GB.** All-CPU-expert concurrent total
  ≈ 113.5 GiB (§8.4). Forcing a fit means parking ≥12 expert layers on GPUs,
  half of them naturally on the thermally suspect P100. That is a forced
  fit, not a configuration this design recommends as a default.
- **CPU-offload speed has a measured failure mode.** On comparable x86
  hardware (RTX 3090 + EPYC, llama.cpp, heavy CPU-expert offload) generation
  measured **1.41–1.81 t/s** (VERIFIED as reported); an optimized mixed
  `--fit` placement on desktop hardware reached ~25 t/s. Prometheus estimate
  for the §8.6 solo configuration: **8–20 t/s, ESTIMATE** — budget at the
  bottom until measured. Vendor reasoning numbers are excellent (AIME25 92.5
  at high effort, Tau-Bench Retail 67.8 — VERIFIED vendor claims), and
  **medium reasoning effort** is the practical orchestration setting (the
  card's medium column: AIME24 80.4, Tau-Bench Retail 62.0, at a fraction of
  the trace length).

**Where it runs:** solo with the coder unloaded at 96 GB (§8.6), or
concurrently after the 128 GB upgrade (§8.4). If P100 temperatures are not
confirmed, its solo mode degrades to A4000-only all-CPU experts (the ~1.6
t/s regime) — at that point the honest recommendation is to stay on the
primary and treat GPT-OSS as post-upgrade-only.

### 8.4 RAM arithmetic (the concurrent case, done explicitly)

Reserves are ESTIMATES to be replaced by Phase-0 measurements; expert
figures are DERIVED from VERIFIED file sizes (UD quants hold
embeddings/attention at higher precision, so true expert byte share is
slightly below the parameter share — the primary is charged at 96%, not
flattered). KV caches live in VRAM under GPU offload, not in these figures.

| Line item | GiB | Label |
|---|---|---|
| Coder experts in RAM (Coder-Next, `--n-cpu-moe 46`) | 43.5 | from Pete's live config |
| Orchestrator experts in RAM (Next-Thinking UD-Q3_K_XL, `--n-cpu-moe 48`) | 31.7 | DERIVED/ESTIMATE (33.06 GiB file × ~0.96) |
| Windows + LLMdil app + gateway + Chatbox + drivers reserve | 10.0 | ESTIMATE — measure at idle |
| CPU compute buffers, mmap/page-cache pressure, both servers | 2.0 | ESTIMATE |
| **Total** | **87.2** | |
| **Headroom vs 96 GiB** | **≈ 8.8 GiB** | ESTIMATE |

Policy inside this budget: **no third large backend co-loaded.** Loading
`tool-runner` or `coder-fast` seat models (§5.1) or the Nemotron backend
subtracts their expert RAM from the headroom. Solo orchestrator ≈ 43.7 GiB
total. Swap must never be reached in either mode; if Phase-0 measurement
shows the OS reserve above ~14 GiB, the concurrent case drops to ~4 GiB
headroom and the correct response is the 128 GB upgrade, not a smaller
quant.

For the record: GPT-OSS concurrent (all-CPU experts) = 58.0 + 43.5 + 12.0 =
**113.5 GiB > 96 GiB**. Qwen3-235B concurrent at its smallest defensible
quant (Q2_K, 85,691,001,504 B, VERIFIED) = experts ≈ 77.1 + 43.5 + 12.0 =
**132.6 GiB**. DeepSeek-R1's smallest published GGUF (UD-IQ1_S,
140,231,438,464 B, VERIFIED) is larger than the machine's entire RAM.

**What the 128 GB upgrade unlocks (VERIFIED sizes, DERIVED shares):**

1. **A better quant of the primary — the clean unlock.** Next-Thinking
   UD-Q4_K_XL: 45,932,667,040 B (42.78 GiB), experts ≈ 41.1 GiB; concurrent
   total ≈ 96.6 GiB — note this *just misses* 96 GiB, which is the sharpest
   statement of why the upgrade matters — and fits 128 GiB with ~31 GiB
   headroom. UD-Q5_K_XL (56,664,760,672 B) also becomes concurrent-viable.
2. **GPT-OSS-120B concurrent** in its clean all-CPU-expert form (113.5 GiB)
   fits with ~14 GiB headroom.
3. It does **not** unlock the 235B for the concurrent seat: Q3-class
   experts alone (≈ 91 GiB) + coder 43.5 GiB = ~134.5 GiB > 128 GiB. It
   unlocks 235B Q3 *solo*, barely — still rejected on speed (§8.7).

### 8.5 Model verification list (feeds §10 Phase 0)

1. Idle RAM reserve on Prometheus (validates the 10 GiB ESTIMATE) and idle
   P100 temperature (gates §8.3/§8.6 fallback placement).
2. Primary: local SHA-256 of the downloaded GGUF vs §8.2; load with the
   §8.6 flags alongside the coder; read back both backends' RAM residency
   and A4000 VRAM; confirm the §8.4 total with headroom intact.
3. Primary: `llama-bench` generation/prompt rates vs the §8.2 ESTIMATE
   range; a timed end-to-end delegation-style prompt (plan + one tool call)
   as the real metric.
4. Primary: tool-call smoke test — two-tool prompt via
   `/v1/chat/completions`, confirm structured `tool_calls` in the response
   and the think trace confined to `reasoning_content`. Failure → Instruct
   variant (§8.2) → then fallback.
5. Fallback (only if invoked): a harmony round-trip through the actual
   client/LiteLLM path, including a tool result returning on the commentary
   channel.
6. Each specialist group: the `mcp-capabilities-design.md` Phase 0 probe
   (§2.5) — a group that hasn't passed its probe is not a delegation target.

### 8.6 Backend configuration sketches (LLMdil terms)

**PRIMARY — Qwen3-Next-80B-A3B-Thinking, UD-Q3_K_XL**

- **Placement: A4000 only** (CUDA 0, split-mode none, device restricted to
  CUDA0 — the `CUDA_VISIBLE_DEVICES`-style restriction LLMdil already
  applies to placed backends). The P100 is not used by this backend.
  GPU-side footprint: non-expert weights ≈ 1.4 GiB of file share, ESTIMATE
  2.5–4 GiB total at UD precision; KV at 65,536 with q8_0 ≈ 0.75 GiB
  (DERIVED); compute buffers ~1–2 GiB (ESTIMATE) — roughly 5–7 GiB of the
  A4000's 16 GiB, leaving the rest for the coder backend's GPU tensors.
  Phase 0 must read back actual VRAM for both backends loaded.
- **Flags:**
  ```
  -ngl all                 # every layer's non-expert tensors on GPU
  --n-cpu-moe 48           # ALL 48 layers' routed experts in system RAM.
                           # Derivation: the model has 48 layers (VERIFIED);
                           # 48 = full CPU-expert offload = minimum VRAM,
                           # maximum RAM (31.7 GiB — what §8.4 budgets).
                           # Phase-0 tuning knob only: lowering to 36 moves
                           # ~8 GiB of experts to the A4000 (faster, +8 GiB
                           # VRAM, −8 GiB RAM) if VRAM headroom is MEASURED.
  -c 65536                 # configured context; native is 262,144
  -fa on                   # flash attention
  -ctk q8_0 -ctv q8_0      # KV quantization; halves the (already small) KV
  --jinja                  # REQUIRED for tool calling (llama.cpp docs)
  --reasoning-format deepseek   # think trace -> reasoning_content
  --threads 12             # physical cores; SMT siblings left to coder/OS
  --parallel 1             # one slot: full context for the session
  ```
  Sampling (VERIFIED, card): temperature 0.6, top_p 0.95, top_k 20, min_p 0.
  Per-request output cap: start at 8,192 tokens, plus a reasoning budget —
  the card's 32,768-token recommendation is for benchmark problems, not
  delegation turns. Chat template: the GGUF's embedded Jinja template,
  unmodified (the card states it already implements the "no thinking content
  in history" practice). Do not enable YaRN/1M context; the card warns
  static YaRN can degrade shorter-text performance.
- **Catalog note:** this is a new catalog/backend entry in LLMdil (an
  orchestrator backend + its gateway group — new group or the `planner`
  group re-pointed; §11, decision 1). Context preset shown to Pete should be
  65,536 against a rated maximum of 262,144, per the existing catalog
  conventions.

**FALLBACK — GPT-OSS-120B, MXFP4 (solo at 96 GB; concurrent after 128 GB)**

- **Placement (solo):** layer-split across both GPUs (`-sm layer`) — with
  the explicit gate that the P100 half is enabled only after Pete confirms
  sane P100 idle temps; until then, A4000-only with `--n-cpu-moe 36` and the
  slow regime accepted, or the fallback is not run.
  ```
  -ngl all
  --n-cpu-moe 22           # Derivation: 36 layers (VERIFIED); expert layer
                           # ≈ 1.64 GiB (DERIVED). 22 on CPU ≈ 36 GiB RAM
                           # (solo total ≈ 48 GiB — fits); 14 on GPU
                           # ≈ 23 GiB expert VRAM + ~4–6 GiB non-expert
                           # (ESTIMATE) + KV ≈ 1.5 GiB at 32K ≈ 29–31 GiB
                           # vs 32 GiB across both cards. Tight by
                           # construction; if load OOMs, raise to 24 and
                           # take the speed hit — do not lower context first.
  -c 32768                 # native 131,072; 32K is the delegation size
  -fa on
  --jinja                  # native GPT-OSS harmony handler (VERIFIED)
  --chat-template-kwargs '{"reasoning_effort":"medium"}'
  --threads 12
  --parallel 1
  ```
  After the 128 GB upgrade, the concurrent form is the same file with
  `--n-cpu-moe 36` (all experts in RAM; GPUs carry non-experts only; the
  P100 expert load disappears) alongside the coder.

### 8.7 Rejected candidates, plainly

- **Qwen3-235B-A22B (Instruct-2507 and Thinking-2507).** The strongest
  vendor scores in the set (Thinking: AIME25 92.3, GPQA 81.1, BFCL-v3 71.9 —
  VERIFIED vendor claims). Unreachable here: the smallest defensible GGUF
  is Q2_K at 85,691,001,504 B (VERIFIED); concurrent experts alone ≈ 120.6
  GiB before reserves. An IQ1_M exists (53,226,813,664 B, bartowski,
  VERIFIED) but ~1.5 bpw is an extreme quant whose failure mode is exactly
  degraded structured output — disqualifying for a seat whose whole job is
  reliable structured output. Speed under offload at fitting quants:
  ~3–6 t/s generation, ESTIMATE (22B active, ~3.8 GB/token of expert bytes
  at Q2) — with Thinking's long traces, a single planning pass is minutes.
  The 128 GB upgrade does not save it for the concurrent seat (§8.4).
- **DeepSeek-R1 / V3 (671B).** Excluded by arithmetic, not preference: the
  smallest published GGUF (UD-IQ1_S, 140,231,438,464 B) exceeds total system
  RAM, and 37B active under CPU offload would be slower than the 235B case.
- **DeepSeek-R1-Distill-Qwen-32B.** Fits trivially (Q4_K_M 19,851,335,584 B,
  VERIFIED) and is still rejected: a 32B *dense* model on an older Qwen2.5
  base is the same class as the incumbent 27B planner — not a step up — and
  llama.cpp's function-calling doc's only remark on the R1 family is "seems
  reluctant to call any tools" (VERIFIED), the opposite of what this seat
  needs.

### 8.8 Comparison table

Sizes VERIFIED (HF API). Benchmarks are vendor card numbers at full
precision (VERIFIED as vendor claims).

| | Next-80B-A3B-Thinking **(PRIMARY)** | Next-80B-A3B-Instruct | GPT-OSS-120B **(FALLBACK)** | Qwen3-235B-Thinking-2507 | DeepSeek-R1 |
|---|---|---|---|---|---|
| Total / active | 80B / 3B | 80B / 3B | 116.8B / 5.1B | 235B / 22B | 671B / 37B |
| Layers / experts / per-token | 48 / 512 / 10 +1 shared | same | 36 / 128 / 4 | 94 / 128 / 8 | 61 / 256 / 8 +1 shared |
| Native context | 262,144 | 262,144 | 131,072 | 262,144 | 163,840 |
| Chosen GGUF | UD-Q3_K_XL, 35,494,473,888 B | UD-Q3_K_XL, 35,637,611,296 B | MXFP4, 63,387,346,208 B | Q2_K, 85,691,001,504 B | UD-IQ1_S, 140,231,438,464 B |
| Expert RAM, full `--cpu-moe` | ≈31.7 GiB (DERIVED/EST.) | ≈32.0 GiB | ≈58.0 GiB (DERIVED) | ≈77.1 GiB (DERIVED) | >130 GiB |
| Concurrent with coder @ 96 GB | **Yes (~8.8 GiB headroom, EST.)** | Yes | No | No | No |
| AIME25 (vendor) | 87.8 | 69.5 | 92.5 (high effort, no tools) | 92.3 | — |
| GPQA (vendor) | 77.2 | 72.9 | 80.1 (Diamond, high) | 81.1 | — |
| Agent (vendor) | BFCL-v3 72.0 | BFCL-v3 70.3 | Tau-Bench Retail 67.8 | BFCL-v3 71.9 | — |
| llama.cpp tool-call parsing | Jinja + Qwen3-family/generic — **Phase-0 verify** | same | **Native harmony handler** | Jinja, Qwen family | "reluctant" (docs) |
| Generation speed on Prometheus | 12–25 t/s **ESTIMATE** | Higher (no think trace) **EST.** | 8–20 t/s **EST.**; 1.41–1.81 t/s VERIFIED in all-offload x86 test | 3–6 t/s **ESTIMATE** | n/a |
| Blocking issue | Parsing must be smoke-tested; forced-think latency | Not a reasoning model | Cannot shrink; harmony adapter; offload speed cliff | RAM + speed | Size |

---

## 9. Worked example: "add retry logic to the fetch function and make the tests pass"

The teaching trace. Cast: **Pete** (user), **Orchestrator** (in Chatbox),
**Delegation server** (client machine), **Specialist** (gateway group
`coder`, driven by the sub-loop), **Local tools** (executed by the server on
the client). Workspace: a Rust project; `src/net/fetch.rs` contains
`pub fn fetch(...)`; tests live in the `net` crate.

Visibility key per step: **[Pete]** sees it in Chatbox · **[Orch]** in the
orchestrator's context · **[Spec]** in the specialist's context · **[Log]**
only in the transcript file.

**Step 1 — Pete's prompt.** [Pete][Orch]
Pete, in Chatbox (provider = gateway, model = orchestrator group,
tool-capable on):

> In the sea-whisper-app project, add retry logic to the fetch function in
> the net crate — transient failures should retry a few times — and make
> the tests pass.

The orchestrator's context at this moment: Chatbox's system prompt, this
message, and its tool list — including the three `delegate_*` MCP tools
(§3.3). It has NOT seen any project files. Nobody has read any code yet.

**Step 2 — Orchestrator plans and delegates.** [Pete][Orch]
The orchestrator reasons (visible in Chatbox): the task is a multi-step code
change with a test gate — squarely `delegate_code_implementation`, not a
quick fix (retry semantics are a small design decision) and not a tool task
(files must change). It emits **one MCP tool call** and no final answer yet:

```json
// MCP tools/call, as rendered in the Chatbox transcript:
delegate_code_implementation
{
  "task": "Add retry logic to the fetch function in the net crate (src/net/fetch.rs). Transient failures (connection errors, HTTP 5xx) should be retried up to 3 times with backoff; client errors (HTTP 4xx) must not be retried. Pete's project is at the workspace root.",
  "acceptance_criteria": [
    "cargo test -p net passes",
    "fetch retries transient failures up to 3 attempts",
    "fetch does not retry HTTP 4xx responses"
  ],
  "context_hints": ["src/net/fetch.rs", "net crate"]
}
```

What the orchestrator cannot see: anything about how the work will be done.
What Pete sees: the tool name (the role decision, §3.2) and the spec (the
delegation decision) — the two judgments worth auditing, both on the surface.

**Step 3 — Server setup.** [Log]
Chatbox's MCP client forwards the call over stdio. The server: validates
arguments (schema OK); loads role config (`coder`, caps 25 iter / 20 min);
scope = workspace root `D:\dev\sea-whisper-app`; runs
`git status --porcelain` (clean) and records HEAD `9c41e02`; opens
transcript `…\transcripts\2026-10-17\141203-code_implementation-a3f9.jsonl`;
writes `run_start` with the full spec, the role prompt's hash, and
pre-state. Pete sees none of this unless he tails the log — by design;
Chatbox shows the tool call as "running."

**Step 4 — Sub-loop iteration 1: orientation.** [Spec][Log]
Server → gateway:

```json
POST http://192.168.5.23:4000/v1/chat/completions
{ "model": "coder", "max_tokens": 4096, "tool_choice": "auto",
  "messages": [
    {"role":"system","content":"<code_implementation prompt, §5.3> … Workspace root: D:\\dev\\sea-whisper-app …"},
    {"role":"user","content":"TASK\nAdd retry logic…\n\nACCEPTANCE CRITERIA\n- cargo test -p net passes\n- …\n\nCONTEXT HINTS…\n\nCONSTRAINTS…"}
  ],
  "tools": [ "/* the role's local tools, §5.2, OpenAI shape, §4.2(a) */" ] }
```

Specialist replies with `tool_calls` (abbreviated):

```json
{"role":"assistant","content":"I'll read the fetch implementation and its tests first.",
 "tool_calls":[
   {"id":"call_01","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"src/net/fetch.rs\"}"}},
   {"id":"call_02","type":"function","function":{"name":"search_code","arguments":"{\"pattern\":\"fn fetch\",\"glob\":\"src/**/*.rs\"}"}}]}
```

The specialist's context contains ONLY the system prompt, the task spec, and
now its own actions. It has never seen Pete's message or Chatbox. Note it is
*gathering its own file context* (§4.3) — Step 1's conversation spent zero
tokens on file contents.

**Step 5 — Tool execution 1.** [Spec][Log]
The server executes both calls locally: `read_file` returns 212 lines (fits
the default window); `search_code` finds `fetch` defined once, called from
`src/sync.rs`. Results return as two `tool` messages (§4.2(c)). Elapsed:
~15 ms. Nothing has touched the network except the one gateway call; nothing
has touched Prometheus except inference.

**Step 6 — Iteration 2: read the tests, then edit.** [Spec][Log]
Specialist reads `src/net/fetch_tests.rs` (one call), then — picture formed —
issues the edit:

```json
{"name":"edit_file","arguments":"{\"path\":\"src/net/fetch.rs\",\"old_text\":\"    let resp = client.execute(req)?;\n    resp.error_for_status()?\",\"new_text\":\"    let mut attempt = 0;\n    loop {\n        attempt += 1;\n        match client.execute(req.try_clone()…) { … }\"}"}
```

Server: canonicalizes the path (inside scope ✓), exact-match check:
`old_text` occurs exactly once ✓, applies the edit, returns
`OK: replaced 2 lines with 19 lines in src/net/fetch.rs`. Internal
`files_changed` tally: 1.

**Step 7 — Iterations 3–4: build, first failure.** [Spec][Log]
Specialist calls `run_command {"command":"cargo test -p net"}`. Server
validates against the role allowlist (`cargo` ✓, no shell metacharacters ✓),
runs with `cwd` = workspace, 300 s timeout, key-scrubbed env (§7.3). Result
after ~40 s:

```json
{"role":"tool","tool_call_id":"call_07",
 "content":"exit_code: 101\n… Compiling net …\nerror[E0382]: borrow of moved value: `req`\n  --> src/net/fetch.rs:57:22\n…\ntest result: FAILED (build error)"}
```

An error — and per §6.5 it is *fed back*, because it is exactly the kind a
competent agent recovers from. The sub-loop's whole value is in the next
step.

**Step 8 — Iteration 5: fix and re-verify.** [Spec][Log]
Specialist reads lines 45–70 of the edited file (its own prior edit is on
disk; the read keeps its context honest), then `edit_file` again:
restructures to build the request inside the loop via a closure, so no value
is moved across attempts. Then `run_command {"command":"cargo test -p net"}`
again → `exit_code: 0`, tail: `test result: ok. 41 passed; 0 failed`.

Wait — 41 passed, but where is the retry behavior itself tested? The
specialist adds tests in iteration 6 (`edit_file` on `fetch_tests.rs`: a
mock transport failing twice with 503 then succeeding, asserts 3 attempts;
plus a 404 case asserting 1 attempt), re-runs `cargo test -p net` →
`42 passed; 0 failed`. Acceptance criteria 2 and 3 now have evidence, not
just plausibility.

**Step 9 — Completion and the verification gate.** [Spec][Log]
Specialist replies with no tool calls:

> Implemented bounded retry in fetch(): up to 3 attempts, exponential
> backoff 200ms×2^n with jitter, retrying connect errors and 5xx only; 4xx
> returns immediately. Added two tests covering retry-on-503 and
> no-retry-on-404. cargo test -p net: 42 passed, 0 failed.

Server's gate (§4.4): last file edit was iteration 6; a passing verification
run (exit 0) occurred after it ✓; no nudge needed. Loop ends. `attempts`:
9 iterations, 14 tool calls, 214 s.

What the specialist never saw: Pete, the conversation, the other two roles,
the gateway key (scrubbed even from child env), anything outside the
workspace. What it produced beyond this summary — full reasoning, every
intermediate output — exists only in [Log].

**Step 10 — Result contract returns.** [Pete][Orch]
The server assembles §4.5 from its own records — `files_changed` from
`git diff --numstat` vs the Step-3 snapshot (2 files, +62/−6),
`verification` from the last cargo run, `acceptance` checked against
criteria with evidence strings — and returns it as the MCP tool result. It
is, byte for byte, the JSON sketched in §4.5 (this example is that object).
Size: ~1.9 KB — well under the §4.5.1 bound, no summarization pass needed.
Chatbox appends it to the orchestrator's context as the tool result and
renders it in the transcript under the Step-2 call. **This object is the
entirety of what crosses back.** Not the 212-line file, not the compiler
error, not the 42 test names — those are in the transcript file named by
`transcript_path`, one paste away for Pete.

**Step 11 — Orchestrator's final answer.** [Pete][Orch]
The orchestrator, whose context now contains Pete's request, its own
delegation, and the result contract, synthesizes:

> Done. `fetch()` in `src/net/fetch.rs` now retries transient failures
> (connect errors, 5xx) up to 3 times with exponential backoff and jitter;
> 4xx responses return immediately without retrying. Two tests were added
> (retry on 503, no retry on 404). `cargo test -p net`: 42 passed,
> 0 failed. Full sub-loop transcript:
> `…\2026-10-17\141203-code_implementation-a3f9.jsonl`

It did not re-verify anything itself — its confidence rests on the
contract's `verification` block and the gate that produced it. That is
precisely the trust structure this design intends Pete to be able to
inspect: claim → contract field → transcript line.

**Counterfactual branches** (so the trace also teaches the failure grammar):

- If Step 8's fix had failed 3× identically → §6.3 escalation, result
  `blocked / no_progress`; the orchestrator tells Pete what was tried, with
  the diff so far listed in `files_changed`.
- If `coder`'s backend had emitted `<tool_call>` text instead of structured
  calls at Step 4 → no scraping (§6.5); the run ends
  `failed / unparseable_tool_calls` after repairs, and the fix Pete pursues
  is the Phase 0 probe/template path in the capabilities design — not a
  prompt tweak.
- If Pete had asked only "do the net tests pass right now?" → Step 2 becomes
  `delegate_tool_task`, Steps 4–9 run with no write tools in the list, and
  Step 10's `files_changed` is `[]` by construction.

---

## 10. Build phases

MVP first: one role end to end, then the rest. Each phase has an exit
criterion; later phases assume earlier criteria are actually met, not
scheduled.

**Phase 0 — Preconditions and measurements** (no server code yet)
- Run the `mcp-capabilities-design.md` Phase 0 probes for the `coder` group
  (and per group as roles are added): structured tool calls through the
  gateway, verified (§2.5).
- Load the orchestrator backend (§8.6 primary sketch) alongside the coder:
  verify SHA-256, read back RAM/VRAM residency vs §8.4, run the §8.5
  items 1–4 (idle reserve, `llama-bench`, timed delegation-style prompt,
  tool-call smoke test).
- Check Chatbox's behavior under a long MCP tool call on Pete's machine:
  is there a host-side timeout, and does it render MCP progress
  notifications? This bounds §6.1's wall-clock caps — if Chatbox caps calls
  at, say, 120 s, caps must shrink to fit under it, or the contract needs a
  polling shape (explicitly NOT designed here; flag, don't improvise).
- Exit: `coder` passes its probe; the orchestrator emits clean structured
  delegation-shaped tool calls through the gateway; measured speeds are on
  record (replacing §8.2's ESTIMATEs with facts).

**Phase 1 — MVP: server skeleton + `code_implementation` only**
- Python + FastMCP project (§7.1), `setup.ps1`, config loading (§7.2),
  workspace enforcement (§7.3), role config with the one role (§7.4).
- The sub-loop (§4.1) with the full local tool set for this role (§5.2),
  caps (§6.1), the verification gate (§4.4), result contract (§4.5), JSONL
  transcripts (§4.6), and the failure paths for gateway-unreachable and
  unparseable output (§6.5).
- Chatbox registration (§7.5); run §9's worked example live against a real
  project.
- Exit: the worked example (or its real equivalent) completes end to end;
  Pete can open the transcript and follow every step; a deliberately broken
  delegation (bad spec, unreachable gateway) produces the designed
  `blocked`/`failed` results, not a crash.

**Phase 2 — `tool_task` role**
- Add the read-only role (§5.2): no write tools, wider command allowlist in
  config, its system prompt (§5.3).
- Requires the `tool-runner` seat populated (§5.1) and probed (§2.5) — or a
  config remap to an existing group as an interim exercise, clearly labeled
  as such in the role table.
- Exit: an evidence delegation ("do the net tests pass, and what is the
  tree state?") returns structured evidence with `files_changed: []`, and
  the implement→verify pattern (implement with role 1, verify with role 2)
  works in one Chatbox session.

**Phase 3 — `quick_fix` role**
- Add the role with the `max_files_changed` scope guard (§5.2, §6.4) and
  its tighter caps.
- Requires the `coder-fast` seat populated with its own model (catalog
  selection) and probed; until then the role may run remapped to `coder`
  via config.
- Exit: a one-file fix completes inside the tight caps; a deliberately
  under-scoped fix (real fix needs 4 files, cap is 2) escalates
  `blocked / scope_exceeded` instead of expanding.

**Phase 4 — Hardening and tuning** (ongoing, evidence-driven)
- Retune §6.1 caps and §6.3 thresholds from accumulated transcripts;
  decide transcript retention (§4.6) once behavior is understood.
- Result summarization fallback (§4.5.1 step 2) if field caps prove
  insufficient in practice; the git safety ref (§6.5) if mid-run commits
  have bitten; MCP progress notifications if Phase 0 showed Chatbox renders
  them.
- Revisit the context-guard compaction (§6.1) with real history sizes.
- Exit: none — this phase is the learning loop itself. The Rust port
  decision (§7.1) is revisited here, with months of stable behavior as its
  trigger.

---

## 11. Open decisions for Pete

1. **Orchestrator group identity.** Does the new Qwen3-Next backend
   (§8.2/§8.6) re-point the existing `planner` gateway group, or get a new
   `orchestrator` group (leaving the 27B planner in place as a lighter
   planning option)? Design leans new group — it keeps the incumbent
   available for comparison while learning, and group identity is what
   Chatbox's provider config points at.
2. **Orchestrator variant.** Thinking (recommended) vs Instruct (§8.2) if
   Phase-0 measurement shows forced-think latency or tool-emission problems.
   Same-size swap; decide from Phase-0 data, not in advance.
3. **Seat population for `coder-fast` and `tool-runner`.** Which small
   models (catalog picks), and their placement. Until decided, Phases 2–3
   run on config remaps. Relatedly: is a third co-loaded backend acceptable
   against §8.4's headroom policy when those seats load?
4. **Run the GPT-OSS fallback at all before the 128 GB upgrade?** Design
   position: no — it is solo-only at 96 GB, its interesting placement is
   gated on P100 temperatures, and the primary covers the seat. It stays on
   the shelf as the documented escape hatch if Qwen3-Next tool parsing
   disappoints in Phase 0.
5. **P100 temperature confirmation.** Gates: any fallback use of the P100,
   and §8.6's `--n-cpu-moe 36` tuning knob is A4000-only, so the primary
   does not depend on this — but the fallback's viability does.
6. **128 GB RAM upgrade timing.** Unlocks the primary at UD-Q4_K_XL and
   GPT-OSS concurrency (§8.4). Not required for the MVP; the design runs at
   96 GB as specified.
7. **Master key source.** Chatbox env block (v1 default, §7.2) vs Windows
   Credential Manager as the primary from day one. Both are specified; the
   choice is Pete's comfort with where Chatbox stores its settings.
8. **Transcript retention.** Keep-forever during learning (recommended,
   §4.6) vs 30-day pruning from the start.
9. **A fourth role later?** Design position: no generic `delegate` escape
   hatch (§3.2); new roles arrive as new tools via config (§7.4) — e.g. a
   `delegate_code_review` on `coder-nvidia`. Recorded as a position for
   Pete to overrule, not a gap.
10. **Parallel delegations.** v1 is one-at-a-time (§4.1). Whether to ever
    allow concurrent sub-loops — and against which workspace-conflict
    rules — is deferred until single-loop behavior is understood from
    transcripts.

---

*Evidence and method: server design from Investigator A draft
(`~/workspace/.tmp-assets/delegation-design/investigator-a.md`); model
selection from Investigator B research
(`~/workspace/.tmp-assets/delegation-design/model-research.md`), primary
sources: Hugging Face model cards and API file metadata, llama.cpp
documentation and source, fetched 2026-10-17. Established context per Pete's
direction of 2026-10-17; gateway and Chatbox facts verified the same day in
the MCP capabilities workstream.*

# Routing Rules — Design: the Context-Overflow Rule

Status: shipped — built as designed below and merged; in releases
from v0.1.36, present in v0.1.40. The typed
`RoutingRuleKind::ContextOverflow` is in `crates/manager-config`;
generation in `crates/manager-gateway` bakes the per-group window
table into the generated `llmdil_routing.py` pre-call hook and
emits the native `context_window_fallbacks` safety net; with no
enabled rule the generated YAML is byte-identical to the
pre-feature output and no routing file is written. The §1
description of the dormant fragment-only rule is the
pre-implementation state this design started from.
Date: 2026-10-17. Local tree only.

Related: `docs/mcp-capabilities-design.md` (per-key MCP grants, gateway config
generation changes in flight on `feature/mcp-capabilities`),
`docs/design-principles.md` (UI rules cited as "principle N").

## 1. Intent

Pete's direction (2026-10-17): keep Routing Rules as a feature and build the
first real one.

> When a request exceeds the context window of the model it was sent to, route
> it instead to a designated large-context model set up for this purpose. When
> the rule fires, inject an instruction into the request so the responding
> model briefly tells the user they have exceeded an optimal context window
> size and should compact the conversation for higher-quality responses.

Today `RoutingRule` is a dormant config concept: the schema exists
(`crates/manager-config/src/lib.rs:187-195` — `id`, `description`,
`litellm_fragment: serde_json::Value`, `source`), validation checks only for
duplicate ids (`lib.rs:903-909`), the config generator never reads it
(`crates/manager-gateway/src/lib.rs` `generate_config_yaml` has no reference
to `routing_rules` or `litellm_fragment`), and no code path, wizard step, or
UI control ever constructs one. Pete's live config field-verifies the
dormancy: `routing_rules: []` beside his 5 groups. This design turns the
concept into one typed, validated, generated rule kind — and is explicit
about what stays dormant (raw fragments, §6.1).

## 2. Terminology (defined once)

- **Model group** — the name a client puts in the `model` field of a request
  (e.g. `coder`). In the generated LiteLLM config each group is one
  `model_name` with one or more **deployments** under it; a deployment is one
  concrete backend (`litellm_params` pointing at one llama-server).
- **Pre-call hook** — LiteLLM's `CustomLogger.async_pre_call_hook`: a Python
  callback the proxy runs on every request after authentication and before
  routing. It receives the request as a plain dict (`data`) and may return a
  modified dict that the proxy then routes instead. This is the only hook
  type that exists on `CustomLogger` in 1.105.1 (the sync-sounding
  `pre_call_hook` name belongs to the proxy's internal dispatcher, not to the
  handler API).
- **Context-window fallback** — LiteLLM's `context_window_fallbacks` router
  setting: a map from model group to a list of other groups, consulted only
  when a call fails with `ContextWindowExceededError`.
- **Hard window** — the largest prompt (in tokens) a group will actually
  accept, derived from its backends' configured context (§4.1).
- **Optimal window** — a lower, quality-motivated threshold on the same
  group. Crossing it changes nothing about routing; it triggers the advisory
  injection (§5).

Verification labels used throughout §3: **VERIFIED** means traced in the
cited primary source (LiteLLM tag `v1.105.1`, commit
`d665dc1a6315847fcad6cc2c1264ceb47c06abce`; llama.cpp master as fetched
2026-10-17; or the LLMdil tree at the cited file:line). **UNVERIFIED** means
plausible and consistent with the sources but not traced; every UNVERIFIED
item that matters is retired by a named probe in Phase 0 (§10).

## 3. Mechanism investigation

### 3.1 What llama-server does with an over-long prompt

VERIFIED (llama.cpp, `tools/server/server-context.cpp`,
`tools/server/server-task.cpp`, `tools/server/server-common.cpp`):

- With Pete's launch configuration — `-c <n_ctx>` from
  `ServerFlags.n_ctx`, built in `backend_spec`
  (`src-tauri/src/main.rs:1323-1503`); no context-shift flag and no
  truncation flag is ever constructed anywhere in the tree (zero
  occurrences of `ctx-shift`/`ctx_shift` in `crates/`, `src-tauri/src/`,
  `ui/src/`) — an over-long prompt **fails loudly**. At slot admission the
  server compares the tokenized, chat-template-rendered prompt against the
  per-slot context: `if (slot.task->n_tokens() >= slot.n_ctx)` it sends an
  error and releases the slot. There is no silent prompt truncation path;
  chat completions have no truncation parameter.
- The error is HTTP 400 with a stable machine-readable shape:
  ```json
  {"error": {"code": 400,
             "message": "request (7959 tokens) exceeds the available context size (4096 tokens), try increasing it",
             "type": "exceed_context_size_error",
             "n_prompt_tokens": 7959,
             "n_ctx": 4096}}
  ```
  Match on `type`/counts, not the message wording — the wording varies
  between builds (UNVERIFIED for Pete's exact build; Phase 0 probe P1).
- The comparison is **prompt tokens alone**. Requested `max_tokens` is not
  added. Consequence: a prompt that fits but leaves no room for the
  requested output is *not* rejected — generation stops at the ceiling and
  the response is HTTP 200 with `finish_reason: "length"`. Any mechanism
  that wants to route on prompt + output must do that arithmetic itself.
- The limit is **per slot**, `llama_n_ctx_seq`, also capped by the model's
  training context. LLMdil constructs no `--parallel`/`-np` flag, so today
  slot context equals `-c`. If an operator adds `--parallel N` through
  `raw_flags`, per-slot context becomes `-c / N` and any proxy-side window
  arithmetic overestimates headroom — §6.2 validation warns on this.
- llama-server can count a chat request **exactly**, with the chat template
  applied, before serving it: `POST /v1/chat/completions/input_tokens`
  returns `{"input_tokens": N}` using the same request-parsing path as
  `/v1/chat/completions` (`tools/server/server.cpp` route;
  `handle_count_tokens` in `server-context.cpp`). This is the exact number
  the admission check compares against `n_ctx`. (`POST /tokenize` exists
  but tokenizes a raw string without the template, so it undercounts.)

### 3.2 What LiteLLM does with that error: `context_window_fallbacks`

VERIFIED (LiteLLM v1.105.1):

- Configured as `[{"<group>": ["<fallback group>", ...]}]`, keyed by model
  group. Usable from `router_settings` (copied into `Router(**params)` at
  config load, `litellm/proxy/proxy_server.py` L6968–L7005; `Router.__init__`
  parameter at `litellm/router.py:805`) or from `litellm_settings` (applied
  as the `litellm.context_window_fallbacks` global, consumed at
  `litellm/router.py:1116`). Both paths work; §6.3 picks one.
- It is a **post-error** mechanism: the router tries the call, and only in
  the exception handler (`litellm/router.py:7406-7430`) branches on
  `isinstance(e, litellm.ContextWindowExceededError)`. With the setting
  present, the error is not retried in-group first
  (`should_retry_this_error`, `litellm/router.py:7941`).
- Classification of an upstream error is **string matching** on the error
  text (`ExceptionCheckers.is_error_str_context_window_exceeded`,
  `litellm/litellm_core_utils/exception_mapping_utils.py:82-122`). The
  pattern list contains the exact substring `"exceeds the available context
  size"` with the source comment `# llama.cpp/Lemonade`. llama-server's
  message (§3.1) contains that substring, and the mapping chain for an
  `openai/<name>` deployment with an `api_base` is traced end to end:
  provider exception → `exception_type()` (`exception_mapping_utils.py:2470-2483`)
  → `_map_openai_exception` → the context-window check at `:317` raises
  `ContextWindowExceededError` (status 400, `litellm/exceptions.py:535-557`).
  The check is not gated on the upstream status code.
- Two sharp edges, both VERIFIED:
  - If `context_window_fallbacks` is set but has **no entry for the
    requested group**, the original exception is re-raised — the generic
    `fallbacks` chains are *not* then consulted for that error
    (`router.py:7431-7437` covers the unset case falling through).
  - If the setting is absent entirely, a context error falls through to the
    generic group fallback chains.
- What it cannot do: fire at any threshold below hard failure, and modify
  the retried request. The retry re-sends the same messages to the fallback
  group. **Native fallbacks alone therefore cannot deliver Pete's
  requirement** — no optimal-window trigger and no advisory injection.

### 3.3 Preemptive option A: LiteLLM `enable_pre_call_checks`

VERIFIED (LiteLLM v1.105.1, `litellm/router.py:12444-12623`):

- With `router_settings.enable_pre_call_checks: true`, deployment selection
  counts input tokens and filters out deployments whose
  `max_input_tokens` is exceeded. For LLMdil's custom GGUF model names,
  `max_input_tokens` resolves only if set explicitly per deployment in
  `model_info` — the LiteLLM cost map has no entry, the lookup returns
  `None`, and **no filtering happens at all** (`router.py:12387-12400`).
- Filtering is **within the group only**: an oversized-for-one-deployment
  request is silently steered to a larger deployment of the same group. It
  raises `ContextWindowExceededError` — which `context_window_fallbacks`
  can then rescue cross-group — only when *every* deployment in the group
  is too small.
- The count is the weak point (§3.5): the router's counter calls
  `litellm.token_counter(messages=..., tools=...)` **without a model**
  (`router.py:12368`), and counting is fail-open (any counting error leaves
  deployments unfiltered).

Verdict: usable as a blunt within-group guard, but its counting is the
least accurate option available and it offers no injection point. Not
recommended as the mechanism; not needed if §3.4 is adopted.

### 3.4 Preemptive option B: the pre-call hook

VERIFIED (LiteLLM v1.105.1):

- `CustomLogger.async_pre_call_hook(user_api_key_dict, cache, data,
  call_type)` (`litellm/integrations/custom_logger.py:413`). Return
  contract (`litellm/proxy/utils.py:1768-1781`): a returned dict
  **replaces** the request data; returning `None` keeps the original dict —
  and because the dict is passed by reference, in-place mutation also takes
  effect.
- Mutating `data["model"]` is the documented canonical use
  (docs.litellm.ai/docs/proxy/call_hooks: `data["model"] = "my-new-model"`).
  The proxy path is traced: hook runs in
  `common_processing_pre_call_logic`
  (`litellm/proxy/common_request_processing.py:2211`), the result becomes
  `self.data`, and `route_request(data=self.data, ...)` at `:2619` routes
  the mutated request. Mutating `data["messages"]` works by the same
  mechanism — the proxy explicitly re-reads `self.data["messages"]` after
  the hook because guardrail hooks mutate messages in place
  (`common_request_processing.py` L2232–2243).
- The hook runs after authentication and model-alias resolution and before
  routing, for **streaming and non-streaming requests identically** — the
  streaming flag is consulted only after the hook, in response handling.
- Deployment fits LLMdil's generated-config model exactly. Callbacks are
  declared in `litellm_settings` as a dotted path to an **instance**:
  `callbacks: llmdil_routing.proxy_handler_instance` (pointing at the class
  fails config load). Resolution (`get_instance_fn`,
  `litellm/proxy/types_utils/utils.py:9-58`) tries the **config file's
  directory first**: a module file sitting next to `config.yaml` is loaded
  from disk via `importlib` — no `PYTHONPATH` entry and no installed
  package needed. LLMdil already writes `config.yaml` into
  `<data_dir>/gateway/` at gateway start and has the write-at-start pattern
  for generated files (§6.3), so the callback ships as a second generated
  file in the same directory.
- One implementation gotcha, VERIFIED from source: the proxy detects the
  hook via `"async_pre_call_hook" in vars(_callback.__class__)`
  (`litellm/proxy/utils.py:2508`) — the method must be defined **directly
  on the handler subclass**, not inherited through an intermediate class.

### 3.5 Token-counting accuracy

VERIFIED (LiteLLM v1.105.1):

- `litellm.token_counter` has no GGUF tokenizer support. For a custom
  served name it falls back to tiktoken `cl100k_base`
  (`litellm/utils.py:2403-2447`; `_get_default_encoding`,
  `litellm/_lazy_imports.py:94-97`), plus OpenAI chat overhead constants
  (3 tokens/message) regardless of the model's real chat template
  (`litellm/litellm_core_utils/token_counter.py:377-397`). Qwen-family
  tokenizers differ from `cl100k_base` enough that a threshold decision on
  this count alone can be wrong in both directions; no primary source
  quantifies the error (any percentage would be UNVERIFIED).
- The accurate counter already exists on the other side of the gateway:
  llama-server's `/v1/chat/completions/input_tokens` (§3.1) counts with
  the model's real tokenizer *and* its real chat template — the same
  number admission will use. It costs one loopback HTTP call per request.
- Design consequence: the hook counts **exactly, via the backend**, and
  uses `litellm.token_counter` only as a degraded fallback when the
  counting call fails (with a safety margin applied, §4.2), and fails open
  (no reroute, no injection) when neither is available.

### 3.6 Mechanism recommendation

**Primary: the pre-call hook** (§3.4) doing exact counting (§3.5), the
two-threshold decision (§4), the model rewrite, and the advisory injection
(§5) in a single pass, before any upstream call is made.

**Safety net: native `context_window_fallbacks`** (§3.2), emitted for every
covered group → the target group, so that any request the hook misses
(counting endpoint down, hook disabled by its fail-open path, a group
added without regenerating the callback table) is still rescued by
LiteLLM after llama-server's exact admission error. The safety-net retry
carries no injected advisory — acceptable, because it is the rare path,
and §5's once-per-conversation design means a later over-threshold request
in the same conversation will inject then.

Neither mechanism alone meets the brief: native fallbacks cannot inject or
fire at the optimal threshold; the hook alone has no rescue if its count is
wrong in the under-estimate direction on the degraded path.

## 4. The rule, specified end-to-end

### 4.1 Windows

- **Hard window of a backend** = `min(ServerFlags.n_ctx, catalog ctx_max)`
  where `ctx_max` is the model's rated context from the catalog
  (`crates/manager-models/src/lib.rs:43-60`; 0/unknown rating means no cap,
  so the value is just `n_ctx`). `n_ctx` is what llama-server is actually
  launched with (`-c`), and the catalog rating is the cap llama-server's
  per-slot context also applies — so this formula mirrors the server's own
  effective limit (per-slot caveat in §3.1).
- **Hard window of a group** = the minimum hard window over its enabled
  member backends. Routing may land on any member, so the group is only as
  large as its smallest enabled member.
- **Optimal window of a group** = the rule's `optimal_tokens` if set, else
  `floor(optimal_fraction × hard window of the group)`, with
  `optimal_fraction` defaulting to 0.5. The optimal window is a property of
  the *serving* group: after a reroute, the advisory is judged against the
  target's optimal window, not the source's (§4.2 step 4).
- Both windows are computed by LLMdil **at config-generation time** from
  the same config that produces `config.yaml`, and baked into the
  generated callback (§6.3). A change to any backend's `n_ctx` therefore
  updates the rule through the existing pending-changes → activation flow,
  exactly like a `config.yaml` change. The callback contains no window
  arithmetic of its own beyond comparing a count to baked numbers.

### 4.2 Decision procedure (per request, inside the hook)

Scope: chat-completion-shaped requests only (the request data carries
`messages`). All other call types pass through untouched. The hook runs
once per request, at the proxy, before routing — so every step below
happens at most once per request by construction.

1. Let `G` = the requested model (a group name). If `G` is not a group in
   the rule table (e.g. the `openrouter` pseudo-group, or no rule covers
   it), pass through.
2. Count exactly: `POST` the request body to
   `/v1/chat/completions/input_tokens` on the group's representative
   backend (the first enabled member's `api_base` from the generated
   config — loopback by construction; group members serve the same model
   in LLMdil's placement model, so one member's tokenizer is the group's
   tokenizer; heterogeneous `host:port` members are the operator's
   responsibility, noted in §9). Tight timeout (2 s). On any failure, fall
   back to `litellm.token_counter(messages)` × 1.15 margin (estimate path,
   marked as such in the request metadata). On total failure, pass through
   (fail-open).
3. `need` = counted prompt tokens + requested output tokens, where
   requested output = the request's `max_tokens` if present, else the
   rule's `output_reserve_tokens` (default 1,024 — decision D3, §11).
   llama-server ignores the output term (§3.1); the rule includes it
   because a prompt that "fits" with no room to answer produces a
   truncated `finish_reason: "length"` response, which is the failure this
   rule exists to prevent.
4. Decide the serving group `S`:
   - If `need > hard(G)` and `G` is not the target group: `S` = target
     group; rewrite `data["model"] = S`; record
     `data["metadata"]["llmdil_rerouted_from"] = G` (metadata flows into
     LiteLLM's request logs, making reroutes visible in the gateway log
     and distinguishable from direct target traffic).
   - Otherwise `S` = `G`. In particular, a request addressed to the target
     group directly is never "rerouted", and a request that exceeds even
     the target's hard window is still sent to the target (step 5 covers
     the outcome) — the target produces the clearest terminal error
     (§4.3).
5. Advisory: if the rule's advisory is enabled and
   `need > optimal(S)` and the verbatim advisory sentence (§5.2) does not
   already appear in any assistant message in the request's history,
   inject the advisory instruction (§5.1) into the messages (§5.3).
6. Return the (possibly mutated) data. Any exception anywhere in the hook
   → return `None` (fail-open: the original request proceeds untouched).

### 4.3 The designated target, and terminal behavior

- The rule names its target as a **group** (`target_group`), never a
  backend, so membership can change (a second large-context backend, a
  model swap) without editing the rule. Validation (§6.2) requires the
  target to be an existing configured group; the `openrouter` pseudo-group
  is rejected as a target (it is not a `ModelGroup`, and routing oversized
  local traffic to a metered cloud model must never happen by accident).
- Pete's fleet shape today: `planner` (Qwen3.8-27B, rated 262,144) and
  `coder` (Qwen3-Coder-Next, rated 262,144, configured `n_ctx` lower) —
  i.e., the models already rate high enough; what makes a group a useful
  target is a member backend whose **configured** `n_ctx` is raised toward
  the rating. Decision D1 (§11) covers whether the target is a dedicated
  `large-context` group or the `planner` group itself.
- **Target also exceeded:** the request goes to the target (step 4),
  llama-server returns its 400 `exceed_context_size_error` with both
  counts, LiteLLM surfaces it as a `ContextWindowExceededError` 400.
  Terminal behavior is a clear error naming the true sizes — never a loop
  (§9).
- **Target down / no healthy deployment:** the reroute stands and
  LiteLLM's routing error for the target group surfaces. Falling back to
  the source group is not offered: the source is *guaranteed* to fail on
  context for this request, so its error would be strictly less
  informative. Config-time validation warns when the target group has no
  enabled members (§6.2), which is when this is preventable.

### 4.4 Worked examples

Numbers use Pete's current configuration where known (coder's configured
context is 65,536; values are otherwise computed from config at generation
time): source group `coder`, hard window 65,536, optimal 32,768 (50%);
target group hard window 262,144, optimal 131,072. Client sends no
`max_tokens`, so the reserve is 1,024.

1. Chat request to `coder`, counted prompt 40,000. `need` = 41,024 ≤
   65,536 → no reroute. 41,024 > optimal(`coder`) 32,768 → advisory
   injected; the coder model answers and appends the compaction sentence.
   The next request in the same conversation (prompt now 44,000) still
   exceeds the optimal window, but the verbatim sentence is present in
   history → no injection (§5.4).
2. Request to `coder`, counted prompt 70,000. `need` = 71,024 > 65,536 →
   rerouted to the target; `need` ≤ optimal(target) 131,072 → **no
   advisory** (the conversation is large for the coder but nowhere near
   the target's quality-degradation range). The target answers normally.
3. Request to `coder`, counted prompt 150,000 → rerouted to the target;
   151,024 > optimal(target) → advisory injected, target answers with the
   compaction sentence appended.
4. Request to the target directly, counted prompt 270,000 →
   `need` > hard(target) → no reroute exists; the request proceeds to the
   target and fails with the 400 context error carrying
   `n_prompt_tokens`/`n_ctx`. The client sees a clear, correctly-sized
   error, not a truncated answer.
5. Hook's counting call times out → `token_counter` estimate 60,000 ×
   1.15 = 69,000 > 65,536 → rerouted on the estimate path (safe
   direction). If the estimate had come out under the window and the
   request then hit llama-server's admission error, the §3.6 safety net
   (`context_window_fallbacks`: `coder` → target) rescues it post-error,
   without the advisory.

## 5. Context injection

### 5.1 The injected instruction (verbatim template)

One system-role message, with `{need}` and `{optimal}` substituted with
the serving group's numbers (thousands separators, per Pete's number
conventions):

> [Routing note from the local gateway — not from the user, and do not
> quote it: this conversation is now about {need} tokens, beyond the
> roughly {optimal}-token range where your answers are most reliable.
> Answer the user's current request normally and in full. Then, as the
> final sentence of your reply, add this sentence exactly: "{advisory
> sentence, §5.2}" Give this advice only once per conversation: if you
> have already given it earlier in this conversation, do not repeat it.]

### 5.2 The user-visible advisory sentence (verbatim)

> One note: this conversation is now past the length where my answers are
> sharpest — compacting it, or starting a fresh chat with a summary, will
> get you higher-quality responses.

The sentence is fixed verbatim for two reasons: consistent wording for the
user, and it makes the already-advised check in §5.4 a deterministic
string search instead of a second judgment call.

### 5.3 Placement in the message list

Insert the §5.1 message as a system message **at the head of the list,
immediately after any existing leading system messages**. Every chat
template in the fleet renders leading system content; instruction
precedence is highest there; and the user's own messages are never
edited. The alternative — appending the note to the end of the last user
message — is rejected as the primary mechanism because it rewrites the
user's words, and is kept only as the fallback if Phase 0 probe P4 shows
a fleet template mishandling more than one leading system message
(UNVERIFIED today).

### 5.4 "Once per conversation" in a stateless proxy

The proxy sees one request at a time and keeps no conversation state, so
"once" is engineered from the only record that exists — the conversation
history inside each request — by two cooperating mechanisms:

1. **Deterministic pre-check (hook, §4.2 step 5):** before injecting, the
   hook scans prior assistant messages for the verbatim §5.2 sentence.
   Found → the advisory has been delivered; do not inject again. This is
   exact when the model complied with the verbatim instruction, which the
   fixed sentence is designed to make the common case.
2. **Model self-check (inside the §5.1 text):** the instruction itself
   tells the model to check whether it has already given the advice and
   stay silent if so. This covers the cases the string scan cannot:
   the model paraphrased despite the instruction, or the client edited
   the history.

Residual behavior, stated plainly: if the user compacts the conversation
and the compacted history no longer contains the advisory sentence, a
later over-threshold request will advise again — which is correct, since
the new conversation has not been advised. If a client strips assistant
history, both mechanisms degrade to advising on every over-threshold
request; the sentence is one line at the end of an answer, so the failure
mode is mild repetition, not derailment. The model is instructed to answer
the current request "normally and in full" *first* — the advisory never
replaces or shortens the answer.

### 5.5 When injection does not happen

- `need` ≤ optimal(serving group) — including most reroutes (example 2).
- The verbatim sentence is already in the history.
- The hook is on its fail-open path (no count available).
- The rule's advisory toggle is off (reroute-only mode) — the toggle is
  independent so Pete can run silent rerouting.
- Non-chat requests, and requests to groups the rule does not cover.

## 6. Rule representation in LLMdil

### 6.1 Typed rule kinds, not raw fragments

Verdict: **the first rule is a typed kind; raw fragments stay unimplemented
by design.** Argument:

- The fragment path is not a head start — it is pure liability today. The
  generator never emits fragments, validation checks nothing about them,
  and the UI already disagrees with Rust about their type (UI:
  `litellm_fragment?: string`, `ui/src/api.ts:1188,1211`; Rust: arbitrary
  `serde_json::Value`). Making fragments live would mean inventing merge
  semantics for arbitrary JSON into a deterministic, snapshot-tested YAML
  writer — with no validation story and no kill switch beyond deleting
  the rule.
- A typed kind is validatable end to end (§6.2), generatable
  deterministically (the generator emits exactly the artifacts in §6.3 and
  nothing the rule didn't declare), and testable with the repo's existing
  snapshot-test pattern for generated config.
- The kill switch is structural: a disabled or deleted typed rule emits
  **no** `callbacks` entry and **no** `context_window_fallbacks` entries on
  the next activation, so the gateway returns to exactly today's behavior.
  There is no runtime state to unwind.
- Legacy tolerance: a config containing old-shape rules (fragment present,
  no `kind`) must still load. Such rules are *legacy raw rules*: rendered
  read-only in the UI, never emitted — which preserves today's dormancy
  semantics for fragments while typed rules go live. (In practice every
  real config, Pete's included, has an empty list, so this is belt and
  braces.)

### 6.2 Config shape

`RoutingRule` gains a kind and an enable flag; the fragment becomes
optional and deprecated:

```rust
/// A routing rule. Typed kinds are validated and emitted by the gateway
/// config generator; a rule carrying only a legacy `litellm_fragment`
/// is inert (never emitted) — see docs/routing-rules-design.md §6.1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    pub id: String,
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub kind: Option<RoutingRuleKind>,
    /// Legacy raw fragment. Deprecated: retained so old configs load;
    /// never emitted into the generated gateway config.
    #[serde(default)]
    pub litellm_fragment: Option<serde_json::Value>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RoutingRuleKind {
    /// Requests too large for their group are rerouted to a
    /// large-context group; past the optimal window the serving model
    /// is instructed to advise compaction (design §4–§5).
    ContextOverflow {
        target_group: String,
        /// Advisory threshold as a fraction of the serving group's
        /// hard window, in (0, 1). Ignored when `optimal_tokens` is set.
        #[serde(default = "default_optimal_fraction")] // 0.5
        optimal_fraction: f64,
        #[serde(default)]
        optimal_tokens: Option<u32>,
        /// Output tokens assumed when a request sets no `max_tokens`.
        #[serde(default = "default_output_reserve")] // 1024
        output_reserve_tokens: u32,
        #[serde(default = "default_true")]
        advisory: bool,
    },
}
```

Serialized example (Pete's prospective first rule):

```json
{
  "id": "context-overflow",
  "description": "Requests too large for their model go to the large-context group; advisory past the optimal window",
  "enabled": true,
  "source": "user",
  "kind": {
    "type": "context-overflow",
    "target_group": "large-context",
    "optimal_fraction": 0.5,
    "optimal_tokens": null,
    "output_reserve_tokens": 1024,
    "advisory": true
  }
}
```

Validation additions in `manager_config::validate` (alongside the
existing duplicate-id check):

- `target_group` names an existing group and is not the literal
  `openrouter`.
- `optimal_fraction` ∈ (0, 1); `optimal_tokens`, when set, is > 0 and —
  when the target's hard window is computable — below it.
- At most one enabled `context-overflow` rule in v1 (a second is a
  validation error, not undefined behavior; multi-rule composition is a
  later design).
- Warnings (surfaced in the UI, not errors): the target group's hard
  window is not larger than some covered group's (the rule cannot help
  that group); the target group has no enabled members; any covered
  backend's `raw_flags` contain `--parallel`/`-np` with a value > 1
  (per-slot windows would be smaller than the rule's arithmetic, §3.1).

The `description` remains the human label; the UI pre-fills it from the
typed fields (§7) and Pete can edit it.

### 6.3 Generated artifacts

All generation stays in `crates/manager-gateway`'s `generate_config_yaml`
plus one sibling artifact, through the existing write-at-start path.

1. **`config.yaml` — `litellm_settings` gains two keys**, only when at
   least one typed rule is enabled (both omitted otherwise, so a
   rule-free config generates byte-identical YAML to today):
   - `callbacks: llmdil_routing.proxy_handler_instance` — alongside the
     existing `success_callback: ["prometheus"]` and `fallbacks:` keys in
     the same block. (Placement choice: `litellm_settings` is where
     LLMdil already puts `fallbacks`, and the `context_window_fallbacks`
     global-setattr path from this block is VERIFIED at
     `litellm/router.py:1116`.)
   - `context_window_fallbacks:` — one entry per covered group:
     `- coder: [large-context]`, etc. **The target group itself gets no
     entry** (loop safety, §9). Groups are "covered" = every configured
     group except the target and except `openrouter`.
2. **`<data_dir>/gateway/llmdil_routing.py`** — a generated file beside
   `config.yaml`, written by `StartGateway::run`
   (`crates/manager-wizard/src/steps.rs:4131ff`, beside the `config.yaml`
   write at L4228–4250, via the same `ops.write_file` pattern as the
   existing `firewall.cmd` precedent). Contents: a fixed template (the
   handler class of §6.4) with the rule table baked in as a Python
   literal — per group: hard window, optimal window, representative
   counting URL; plus target group, advisory sentence, and toggles. The
   template is a `const &str` in `manager-gateway`, following the tree's
   existing Rust-string-Python precedent (`GATEWAY_BOOTSTRAP`,
   `steps.rs:2999-3000`); only the table varies, and its values are
   serialized (never string-interpolated code), so config content cannot
   inject Python. Header mirrors the YAML's generated-file notice. No
   `PYTHONPATH` is needed (§3.4: config-directory-relative resolution).
   When no typed rule is enabled, the file is not written and any stale
   copy is removed (an unreferenced callback file would be inert, but
   leaving it invites confusion).
3. **Drift/currentness:** `gateway_config_is_current`
   (`crates/manager-wizard/src/lib.rs:752-779`) today regenerates and
   byte-compares only `config.yaml`. It must compare the callback file
   too (present-and-equal when a rule is enabled, absent when not), or
   rule changes would not register as pending changes — and the MCP
   design's spawn fingerprint has the same blind spot, noted there for
   coordination (§8). (Shipped: the wizard check and the activation
   drift check in `crates/manager-app/src/activation.rs` both cover
   the `llmdil_routing.py` sibling.)

### 6.4 The callback, specified

```python
class LlmdilRoutingHandler(CustomLogger):
    async def async_pre_call_hook(self, user_api_key_dict, cache, data, call_type):
        try:
            return _route(data)   # §4.2; mutates data in place / returns it
        except Exception:
            return None           # fail-open, unconditionally

proxy_handler_instance = LlmdilRoutingHandler()
```

Contract points:

- The method is defined **directly on this class** (§3.4 gotcha).
- `_route` implements §4.2 exactly: scope check → count (backend
  `input_tokens` first, margined `token_counter` second, pass-through
  third) → reroute decision → advisory scan + injection → return data.
  It never raises out of the hook, never blocks longer than the counting
  timeout, and never sends the request anywhere except the group's own
  loopback counting endpoint (baked from the generated config — no
  caller-supplied URL, so no SSRF surface).
- The template imports only `litellm` (for `CustomLogger` and
  `token_counter`) and the standard library. A template bug is a
  gateway-start failure (LiteLLM validates callbacks at config load and
  raises — fail-closed at startup), which is why the template is fixed
  code under snapshot tests and the rule table is data, and why Phase 1
  includes a `py_compile` check on the generated file in tests.

## 7. UI

Placement: the Gateway view already has the surface — the Routing rules
card (`ui/src/views/static.ts:3605-3636`, today read-only, omitted when
zero rules) and the "Routing rules" count in the gateway summary
(`static.ts:3424`), which becomes a real number the moment a rule exists.
No new view, no new route.

Design, against `docs/design-principles.md`:

- The card's header row renders always in the Gateway view (it is the
  creation surface, not a data block), carrying an **Add rule** button —
  the surface's standard workflow action, so words + icon (principle 15).
  The card body renders only when rules exist (principle 13). This
  resolves the discoverability tension the audits previously settled by
  omitting the card entirely: with a producer feature shipping, the card
  is a control surface, and the summary count tile Pete chose to keep now
  has a real value behind it.
- One rule row per rule, values as Label: Value (principle 9): `Type:
  Context overflow`, `Target: large-context`, `Optimal: 131,072 tokens
  (50%)`, `Advisory: On`. State chip only for the non-default state
  (principle 11): `Disabled` in gray when off; enabled is the default and
  gets no chip. Enable/disable is an icon-only toggle with tooltip +
  aria-label; **Delete** is destructive, so words + icon (principle 15).
  Explanation (what the rule does, what "optimal" means) lives in an
  info-tip on the card title (principle 5), not inline prose.
- Create/edit uses the codebase's existing add-form pattern (as in the
  add-a-backend card): Kind is fixed to Context overflow in v1 (shown as
  text, not a dead select); Target is a `<select>` of configured group
  names (the pattern of the Network access select,
  `static.ts:3451-3454`); Optimal is a tokens number input with the
  fraction default stated in its info-tip ("Default: half the serving
  model's window"); Advisory is a toggle. Validation warnings from §6.2
  render in the form before save, not after.
- Save path needs **no new Tauri command**: the generic
  `update_gateway_config` merge-patch (`src-tauri/src/main.rs:2960ff`,
  `ui/src/api.ts:1486`) already round-trips `routing_rules`; the change
  then flows through the existing pending-changes → Activation Plan →
  gateway restart path, which regenerates both artifacts (§6.3).
- The Generated config card gains the callback file as a second tab of
  the same card (principle 2: one section, multiple views), shown only
  when a typed rule is enabled.
- The Routing section's Flow view is unchanged; reroutes are observable
  in the gateway log via the `llmdil_rerouted_from` metadata (§4.2) and
  as traffic on the target group, not as a new visualization (principle
  6: no visual element without real data behind it).

## 8. Interactions

- **Group fallback chains.** Independent layer, deliberate precedence
  change for one error class. Chains (`ModelGroup.fallbacks` →
  `litellm_settings.fallbacks`) still govern all non-context failures of
  the *serving* group — including failures of the target group after a
  reroute. But per §3.2, once `context_window_fallbacks` has an entry for
  a group, a context error on that group goes to the context target and
  the generic chain is *not* consulted for that error. Concretely:
  `coder`'s chain is `[openrouter]`, yet a context error on `coder` will
  route to the local large-context group, **not** to OpenRouter. That is
  the semantics Pete asked for (oversized work stays local and gets the
  advisory path), and it is decision D6 (§11) because it is a behavior
  change to an existing chain.
- **OpenRouter tier.** The rule never targets it (validation, §6.2) and
  never fires for requests addressed to it (not a group in the table,
  §4.2 step 1). Its existing role — chain fallback for non-context
  failures, and overflow under the tier's own app-side evaluation — is
  untouched.
- **MCP per-key grants.** The MCP design's grants are `object_permission`s
  on MCP *servers*, keyed to the virtual key
  (`docs/mcp-capabilities-design.md` §5.3); nothing there proposes per-key
  *model* restrictions. The hook runs after auth and rewrites only the
  model; the key and its MCP grants are untouched, and the rewritten
  request executes on another local deployment in the same trust domain.
  One forward constraint, recorded here so it is not lost: LiteLLM
  virtual keys *can* carry model allowlists in general; if LLMdil ever
  issues model-restricted keys, the hook must check the key's allowed
  models before rewriting. Not a v1 feature. Shared-surface coordination:
  the MCP design adds `require_key_mcp_access_defined` to
  `general_settings` and its own generator/start-step changes on
  `feature/mcp-capabilities`; this design adds keys to `litellm_settings`
  and a sibling file. No key collisions, but both branches modify
  `generate_config_yaml` and `StartGateway::run` — build sequencing in
  §10 accounts for it.
- **Streaming.** The hook is verified stream-agnostic (§3.4): rewrite and
  injection happen before routing, so a rerouted streaming request simply
  streams from the target, advisory included in the prompt. The safety
  net also applies to streaming: llama-server rejects at admission,
  before any token streams, and the streaming exception path uses the
  same `exception_type` mapping (`litellm/main.py:767`) — traced in
  source; confirmed on-machine in Phase 0 probe P3.
- **Delegation MCP server.** Its sub-loops call groups through the
  gateway, so an oversized subtask context is rerouted like any other
  request, and the advisory can be injected into a sub-loop conversation —
  where it surfaces as a compaction note in the specialist's result
  summary to the orchestrator. Harmless and arguably useful signal; no
  special-casing in v1.
- **Optimize page.** Consultations go direct to a backend over loopback,
  never via the gateway — unaffected.

## 9. Safety and loop analysis

- **Loop safety is structural, in three independent layers.** (1) The
  hook runs once per request and its decision is single-pass: it can set
  the model to the target, and nothing re-invokes it for that request.
  Router-internal fallbacks do not re-run proxy pre-call hooks (traced
  from the §3.4 code path — the hook sits in proxy request processing,
  fallbacks inside the router call; labeled UNVERIFIED as a composed
  behavior and confirmed by Phase 0 probe P5). (2) The hook itself
  refuses to reroute a request already addressed to the target. (3) The
  generated `context_window_fallbacks` map contains **no entry for the
  target group**, so a context failure *on* the target has nowhere to
  fall back to and propagates as the terminal error (§4.3).
- **Fail-open at runtime, fail-closed only at startup.** Any hook
  exception, counting failure, or timeout leaves the request untouched —
  the rule can degrade to today's behavior but cannot break requests.
  The one fail-closed surface is callback *loading* at gateway start
  (LiteLLM raises on an unloadable callback); §6.4's mitigations (fixed
  template, data-only table, snapshot tests, `py_compile` in tests) plus
  the start step's existing health check and the activation flow's
  rollback are the controls. Disabling or deleting the rule and
  re-activating removes both artifacts and restores today's gateway
  exactly (§6.1) — that is the operator kill switch, on the same
  activation path as every other gateway setting.
- **No new trust surface.** The callback's only outbound call is to a
  loopback backend address baked in by the generator from LLMdil's own
  config. It reads no caller-supplied URL, header, or path. Injected
  content is a fixed template with two integers substituted.
- **Honesty of the mechanism.** The injection is a user-configured,
  user-visible rule (listed in the Gateway view, described in its own
  description field), and reroutes leave the `llmdil_rerouted_from`
  metadata trace in gateway logs. Nothing about it is hidden from the
  operator — it is Pete advising himself, by design.
- **Cost.** One loopback tokenize call per chat request to a covered
  group (plus the request's own tokenization at admission). At this
  gateway's scale that is negligible next to generation; the 2 s timeout
  bounds the worst case, and the degraded path is margined and marked.
- **Known approximation.** Group hard windows assume one representative
  tokenizer per group (§4.2 step 2). Mixed-model groups — only possible
  via hand-added `host:port` members — can make the exact count exact for
  the representative and approximate for others; the safety net covers
  the under-estimate direction, and §6.2's warnings cover the
  configuration hygiene.

## 10. Build phases

**Phase 0 — on-machine probes (retire the UNVERIFIED items before any
build).**
- P1: send an over-long prompt directly to a running backend on
  Prometheus; capture the error body. Confirms `type:
  exceed_context_size_error`, the counts, and the message wording on
  Pete's actual llama.cpp build.
- P2: `POST /v1/chat/completions/input_tokens` against planner and coder
  backends with a real conversation; confirm the count matches the
  admission behavior in P1.
- P3: scratch gateway config (hand-written, throwaway): one small-window
  group, one large-window group, `context_window_fallbacks` between them.
  Confirm an oversized request is rescued, and the same for `stream:
  true`.
- P4: chat-template render check on the planner and coder templates:
  two leading system messages (existing + injected position, §5.3)
  render correctly.
- P5: nested-fallback behavior: make the *target* fail with a context
  error under the P3 config and confirm the error propagates terminally
  (no re-entry, no loop).

**Phase 1 — config + generation.** Typed `RoutingRuleKind` + validation
in `manager-config` (with legacy-fragment tolerance, §6.1–6.2); generator
emits the two `litellm_settings` keys and the callback file in
`manager-gateway` (snapshot tests for YAML and the generated table;
`py_compile` test for the file); `StartGateway::run` writes/removes the
callback file; `gateway_config_is_current` covers it. Sequencing: this
branch touches the same generator and start-step files as
`feature/mcp-capabilities` — land it after that branch merges, or rebase
onto it; do not develop both against the same files concurrently.

**Phase 2 — UI.** Routing rules card becomes the management surface (§7):
add form, rule rows, enable/disable, delete, warnings; Generated config
card gains the callback tab. No new Tauri commands.

**Phase 3 — on-machine validation** against Pete's fleet, mirroring §4.4:
oversized `coder` request reroutes with correct advisory behavior;
direct-to-target oversized request returns the clean 400; streaming
case; a delegation sub-loop case; kill-switch drill (disable rule,
activate, confirm byte-identical-to-today gateway behavior).

## 11. Open decisions for Pete

- **D1 — Target group identity.** Options: (a) a dedicated
  `large-context` group whose initial member is the planner backend with
  its `n_ctx` raised toward 262,144; (b) the `planner` group itself.
  **Recommendation: (a).** The target stays stable if the planner group's
  membership or fallbacks change, direct-to-`planner` traffic remains
  distinguishable from rerouted traffic, and the group's purpose is
  self-describing in the Flow view. Cost: one more group in the list.
- **D2 — Mechanism.** Options: hook + native safety net (§3.6);
  hook only; native only. **Recommendation: hook + safety net.** Native
  only cannot meet the brief (no optimal threshold, no injection); hook
  only leaves the degraded-count path without a rescue.
- **D3 — Output reserve when a request sets no `max_tokens`.**
  Options: 0 (mirror llama-server's admission exactly), 1,024, 4,096.
  **Recommendation: 1,024.** Chat clients typically omit `max_tokens`;
  reserve 0 would route prompts that then truncate mid-answer at the
  ceiling — the failure this rule exists to prevent. The value is
  per-rule editable.
- **D4 — Optimal default.** Options: 40% / 50% / 75% of the serving
  group's hard window, with the per-rule absolute override in all cases.
  **Recommendation: 50%.** Quality degradation from long contexts is
  gradual and workload-dependent; half the window is the point past which
  advising costs nothing and helps often. Tune from real use.
- **D5 — Target-down behavior.** Options: surface the target's routing
  error (recommended); fall back to the source group.
  **Recommendation: surface the error.** The source is guaranteed to fail
  this request on context; its error would be less informative than the
  target's. The preventable case is handled by the §6.2 config warning.
- **D6 — Context errors bypass the OpenRouter chain.** Accept that with
  the safety net configured, a context error routes locally per the rule
  instead of following the group's generic chain to OpenRouter (§8).
  **Recommendation: accept.** It is the requested semantics; all other
  error classes keep their chains.
- **D7 — Advisory when addressing the target directly.** A conversation
  with the target group itself, past the target's optimal window, gets
  the advisory (no reroute is involved). **Recommendation: yes.** The
  advice is about the conversation's length, not about the route taken;
  §4.4 example behavior already assumes it.
- **D8 — Injection placement.** Head insertion after leading system
  messages (recommended, §5.3), contingent on Phase 0 probe P4; fallback
  is appending to the last user message. **Recommendation: head
  insertion**, decided finally by P4's result.

## Appendix — sources

LiteLLM, tag `v1.105.1` (commit `d665dc1a6315847fcad6cc2c1264ceb47c06abce`),
`github.com/BerriAI/litellm`: `litellm/exceptions.py:535-557`;
`litellm/litellm_core_utils/exception_mapping_utils.py:82-122, :2358,
:2470-2483, :275-317`; `litellm/router.py:805, :1116, :7406-7437,
:7574-7600, :7925-7941, :10676-10802, :12333-12623`; 
`litellm/integrations/custom_logger.py:413`; `litellm/proxy/utils.py:1768-1781,
:2368, :2500-2535`; `litellm/proxy/common_request_processing.py:2211,
:2232-2243, :2619`; `litellm/proxy/proxy_server.py:6419, :6628,
:6968-7005`; `litellm/proxy/types_utils/utils.py:9-58`;
`litellm/utils.py:2403-2447, :2518-2550`; `litellm/_lazy_imports.py:94-97`;
`litellm/litellm_core_utils/token_counter.py:56-120, :377-397, :663-690`;
`litellm/main.py:734-740, :767`. Docs: `docs.litellm.ai/docs/proxy/call_hooks`,
`docs.litellm.ai/docs/proxy/reliability`,
`docs.litellm.ai/docs/proxy/config_settings`,
`docs.litellm.ai/docs/completion/token_usage`.

llama.cpp, master as fetched 2026-10-17, `github.com/ggml-org/llama.cpp`:
`tools/server/server-context.cpp` (admission check, `send_error`,
`pre_decode`, `process_token`, `handle_count_tokens`),
`tools/server/server-task.cpp` (error `to_json`, usage serialization),
`tools/server/server-common.cpp` (`format_error_response`),
`tools/server/server.cpp` (routes: `/tokenize`,
`/v1/chat/completions/input_tokens`, `/props`, `/slots`),
`tools/server/server-chat.cpp` (no truncation handling),
`common/arg.cpp` (`--context-shift`, `--jinja`),
`tools/server/README.md`, `tools/server/tests/unit/test_ctx_shift.py`.
Note: master is not necessarily Pete's deployed build — Phase 0 probes
P1–P2 exist to confirm the deployed build's behavior directly.

LLMdil tree: `crates/manager-config/src/lib.rs:45, :120-128, :145-206,
:215ff, :380-402, :416, :445, :772-773, :866-909`;
`crates/manager-models/src/lib.rs:43-60`; `crates/manager-gateway/src/lib.rs:68-77,
:503, :555-580, :618, :654-761, :765`; `crates/manager-wizard/src/steps.rs:2999-3000,
:3029, :3076, :3115-3262, :4131-4334`; `crates/manager-wizard/src/lib.rs:752-779`;
`crates/manager-wizard/src/ops.rs:217, :550, :658, :1118`;
`src-tauri/src/main.rs:1323-1503, :2349, :2384, :2939-2953, :2960ff,
:3734-3756, :4008-4068`; `ui/src/api.ts:908, :1188, :1211, :1231-1260,
:1296-1346, :1365, :1486`; `ui/src/main.ts:24-35, :214-318`;
`ui/src/views/static.ts:1360, :1638, :2550, :3327, :3424, :3451-3454,
:3506-3636, :3670, :3677, :3780-3783`; `ui/src/views/serviceMap.ts:203,
:303`; `docs/design-principles.md` Part 1; `docs/mcp-capabilities-design.md`
§4.2, §5.3, §5.5, §13b.

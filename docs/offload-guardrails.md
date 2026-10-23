# MoE offload guardrails

Why the expert-offload slider has per-model, per-architecture limits, how
they are enforced, and what the operator's options are when a position is
outside them. Implemented Oct 17, 2026 (tracker #37), following the
research in the Oct 17, 2026 incident analysis of the `coder` backend's
CUDA failure.

Status: shipped — present in v0.1.40. The enforcement described
below is in `crates/manager-models/src/guardrails.rs` (catalog pin
and `offload_status` evaluation), the apply-time and launch-preflight
paths in `crates/manager-app`, and the slider rendering in the UI;
the one catalog pin remains Qwen3-Coder-Next-80B on compute
capability (6, 0), endpoints only.

## The incident in one paragraph

The `coder` backend (Qwen3-Coder-Next-80B, UD-Q4_K_M) ran at
`--n-cpu-moe 46` on a Tesla P100 (Pascal, compute capability 6.0). It
served normally for days, then died ~270 ms into a task with
`cudaErrorInvalidArgument` in `ggml_cuda_kernel_launch` — a deterministic
launch-parameter rejection, not OOM. Setting full expert offload
(`-ot exps=CPU`) resolved it. At 46, the routed experts of blocks 46 and
47 (~1.81 GiB) still ran on the GPU; at full offload, no routed-expert
matmul runs on the GPU at all. All 48 of the model's layers are MoE
layers with identical configuration, so nothing is special about the
last two: the risk is a property of GPU-resident routed experts under
partial offload on this build and architecture, and every intermediate
slider position carries the same risk class. The specific defect is
unproven; the configuration is upstream-supported but untested on
sm_60, and llama.cpp has no CI coverage for it.

The slider presented 46 as in-band normal operation: the catalog's
bands priced speed (the default offload is "the most offload that stays
fast"), not verified correctness, and for this model the green limit
equals the maximum. 46 bought 1.81 GiB of GPU residency out of 43.5 GiB
of experts, against 96 GiB of system RAM that holds all experts
comfortably.

## Determination

- **Per model, not global.** A global on/off selector would erase
  genuine partial design points — Nemotron's catalog default is 16 of
  its 24 MoE layers offloaded, by design. The slider stays.
- **Per architecture.** The restriction is a property of the model on
  a GPU architecture, not of the model alone. The same Coder-Next
  partial position is unrestricted on the RTX A4000 (8.6).
- **Exactly one restriction is pinned today:** Qwen3-Coder-Next-80B on
  compute capability (6, 0) — endpoints only (0 layers offloaded, or
  all 48). More restrictions are catalog data, added the same way when
  a model × architecture pair earns one.

## Where the guardrail lives

- **Catalog data.** `ModelSpec.offload_restrictions` in manager-models:
  a list of `(compute_capability, AllowedOffload)` pins, serde-defaulted
  so older data carries none. `AllowedOffload::EndpointsOnly` is the
  only rule kind today. Evaluation is a pure function
  (`manager_models::guardrails::offload_status`): given the model's
  restrictions, the position, the placed GPUs' compute capabilities,
  and whether the launch is CPU-only, it returns Verified, Untested, or
  Blocked. Any placed GPU whose architecture is restricted governs —
  a split across an A4000 and a P100 is a P100 launch for this purpose.
  A placement whose architecture cannot be determined leaves partial
  positions Untested rather than Verified.
- **Apply time.** `backend_set_offload` evaluates the requested
  position against the backend's current placement. An out-of-range
  position is not saved silently: the command refuses, and the
  operator chooses an explicit override or snaps to the nearest
  verified position (for endpoints-only, the nearer endpoint; a tie
  resolves to full offload). There is no silent clamping anywhere in
  the feature.
- **Overrides.** An override is a durable record in the activation
  journal (`offload_overrides`, beside — not inside — the
  pending-change entries, which resolve and prune by design and so
  cannot carry a standing decision). A record names the backend, the
  layer count, and the placed compute-capability set it was granted
  under. It is honored only while all three match: change the value
  or move the backend to a different GPU set and the override no
  longer applies — placement changes re-evaluate the range.
- **Deliberate override operations.** Saving a blocked position
  through the slider (or creating a backend at one) records the
  override as part of that save. A blocked position that arrived
  any other way — hand-edited flags, a model reassignment — has no
  such save to attach to, so two operations manage the standing
  record directly: `backend_set_offload_override` records an
  override for the backend's *saved* position, and
  `backend_clear_offload_override` drops it. The set operation
  writes no config and no flags. Its caller names the exact key it
  confirmed (layers + placed compute-capability set, as the backend
  card renders them); the operation re-derives the key from the
  live config and placement and refuses any mismatch, and refuses
  a position that is not guardrail-blocked — an override covers
  exactly the position it was granted for and never widens. On the
  backend card, the blocked line offers the override beside the
  set-to-verified action in both flags modes (the operation is a
  journal write, not a flags write, so advanced mode is no bar);
  the action takes a confirmation step whose button names the
  exact position — layers and placed CC set — being overridden.
  The overridden state renders distinctly on the card with the
  clear action beside it; clearing returns the position to
  blocked at the next start's preflight.
- **Launch preflight.** `backend_start_preflight` evaluates the
  *effective* offload — the position decoded from the saved raw flags,
  however they got there (slider, flags editor, hand-edited config) —
  and refuses the start when the position is Blocked and no journaled
  override covers it. The refusal names the position, the
  architecture, and the resolving action. This closes the original
  hole: the slider command validated its own writes, but
  `backend_spec` passes raw flags verbatim, so a saved out-of-range
  value previously launched unchallenged. `manager_config::validate()`
  deliberately does not learn about offload: rejecting the config
  itself would break unrelated saves on any machine carrying a legacy
  value, while preflight blocks exactly the launches that are unsafe.

## Rendering

- Verified positions render as before. Positions outside the verified
  range for the placed architecture render **untested** (muted,
  hatched — never green), and the slider readout names the state:
  "Offload: 46 of 48 layers — untested on this GPU", with the
  explanation in the info-tip.
- At rest, the backend card states the position's standing only when
  there is something to say: a light-red "Start blocked" line with
  the one-click resolving action when a start would be refused, an
  amber line when an override is recorded or a partial position is
  unverified, and "Offload: Custom (N of M layers)" when the saved
  flags carry the offload in a hand-written spelling rather than the
  slider's canonical form. The blocked line also carries the
  deliberate-override action and the overridden line the clear
  action (see Overrides above).
- The slider readout prices positions from the same per-block byte
  sums the card's RAM figures use (`sys_bytes_by_position`), so the
  two cannot disagree at partial positions for UD quants, whose
  per-layer sizes are deliberately non-uniform.

## The probe

A start that activates an offload change runs a smoke probe behind it:
once the backend answers, it receives a ~4,000-token prompt — prompt
processing across many ubatches, the phase the fatal task died in —
and the card shows a Probing state until the outcome is known. A
backend that stops during the probe, or a failed probe request, is
surfaced immediately with the verified alternative named.

The probe raises detection odds; it is not a certification. Position
46 ran for days before it failed, so a passed probe never promotes a
position into the verified range. Verified ranges change by catalog
pins, not by probes.

## Optimize experiments

Experiment proposals resolve their offload through the same canonical
translation as the slider (`offload_for_layers`): a proposal at the
model's full MoE layer count launches with `-ot exps=CPU`, never
`--n-cpu-moe <max>` — the same nominal value cannot launch with
different flags depending on the path that wrote it.

# MTP Enablement Plan — Qwen3.8-27B Planner

Status: executed and shipped. The catalog entry shipped in v0.1.37;
the tunable draft-length control shipped in v0.1.40 (tracker #53);
draft acceptance was measured on Pete's machine under v0.1.40, and a
matched-workload comparison captured in a 2026-10-18 diagnostics
bundle has since settled the speed question for the configuration
as run: spec-on decode is a net loss from per-iteration draft
overhead, so MTP stays off for the planner seat. The formal step 5
script run and the `--parallel 1` configuration remain unrun —
see "Shipped status and measured outcome" below.
Goal: enable llama.cpp multi-token prediction (MTP)
self-speculative decoding for the planner role and measure it
against the current baseline before adopting it. The decided
approach is a second model and a second backend — Planner-MTP —
tested side by side with the existing planner. The existing planner
model entry and planner backend are not modified at any point.

## Shipped status and measured outcome (v0.1.40)

- **Catalog (v0.1.37).** The `qwen3.8-27b-mtp` entry below shipped
  with `CATALOG_REVISION 3` in `crates/manager-models`. MTP
  capability is pinned per model by the catalog's `mtp: bool` field
  (`catalog_mtp` in `crates/manager-models/src/lib.rs`); only the
  `-mtp` build carries `mtp: true`.
- **Tunable draft length (v0.1.40, tracker #53).** The backend card
  carries an MTP draft-length control (Off / 1 / 2 / 3 / 4; a
  slider in v0.1.40, a dropdown since the #53 follow-up) backed by the
  `backend_set_mtp` op. State is decoded from, and rewritten into,
  the backend's saved raw flags by
  `crates/manager-models/src/mtp.rs` (`mtp_draft` /
  `set_mtp_draft`): the writer owns only the `--spec-type draft-mtp`
  and `--spec-draft-n-max` tokens and leaves every other flag
  untouched. Enabling without an explicit length writes
  `--spec-draft-n-max 2` (`MTP_DEFAULT_N_MAX`).
- **`--parallel` is never rewritten by the app.** The writer above
  does not touch it. When MTP is on and the saved flags set
  `--parallel` / `-np` to a value other than 1, the UI shows a
  warning that drafting will not occur until the saved flags set it
  to 1. As of v0.1.40 there is still no in-app raw-flags editor, so
  setting `--parallel 1` remains a hand edit of `config.json` (the
  raw-flags instructions in step 3 are superseded for the
  speculative flags by the slider, but not for `--parallel`).
  Before v0.1.40 there was no UI surface for the speculative flags
  at all; the manual raw-flags route in step 3 was the only one.
- **Measured outcome (Pete's machine, v0.1.40).** With the spec
  flags saved through the slider, the speculative path initialized
  and draft acceptance measured 53.9% (132 accepted / 245 generated,
  mean accepted length 2.07) on one workload and 84.0% (79/94, mean
  length 2.68) on another — acceptance is healthy and
  content-dependent. A later diagnostics bundle (2026-10-18,
  v0.1.40) captured the Optimize standardized benchmark itself
  running against `planner-mtp` with the spec flags on — warm-up
  plus three runs per prompt, a matched workload — and settles the
  speed question for the configuration as run: generation measured
  8.3 tok/s (short prompt set) and 10.7 tok/s (long prompt set)
  against 22.5 tok/s plain decode, a net loss at every captured
  point. The loss is per-iteration overhead, not acceptance: each
  draft iteration costs about 250 ms (about 5.7 plain decode steps
  at ~44 ms) while returning 2.07 tokens at 53.9% acceptance and
  2.68 at 84.0%. Every captured run still ran with `n_slots = 4` —
  the `--parallel 1` edit never landed in the saved configuration
  (a later app save overwrote it) — so `--parallel 1` remains
  untested; the iteration cost would need to fall roughly 2–3x for
  it to flip the outcome. The formal step 5 baseline-script run
  has not been executed. Practical standing: MTP off is the
  measured-right setting for the planner seat; the `planner-mtp`
  backend and model are retained as an experiment seat.
- **No MTP for the coder.** Qwen3-Coder-Next has no MTP head, no
  MTP GGUF build exists, and llama.cpp's `draft-mtp` is not
  implemented for the `qwen3next` architecture; its catalog entry
  carries `mtp: false`. There is no coder equivalent of this plan
  until a Qwen3.6-generation coder model exists.

## Current state

- The planner runs Qwen3.8-27B GSQ-RCO IQ3_S on the RTX A4000.
- The installed GGUF is the publisher's base build: 11,771,546,784 bytes,
  851 tensors. It contains no MTP head. The MTP block would be block 64
  (`blk.64.*`, 15 tensors); it is absent from this file.
- Measured baseline (2026-10-17, direct to the backend, 512-token
  generations): 21.89 and 21.67 tok/s generation, about 21.8 tok/s.
- A flag alone does nothing on this file. `--spec-type draft-mtp` makes
  llama.cpp draft with the model's own MTP block; with no MTP block in
  the GGUF there is nothing to draft with.
- The catalog (revision 3) carries a separate opt-in model entry for
  the `-mtp` build: id `qwen3.8-27b-mtp`, name "Qwen3.8-27B
  Planner-MTP (GSQ-RCO IQ3_S)", 12,120,016,960 bytes, sha256
  58fd8267…7a98c12f, disabled by default and unassigned. It reaches
  existing installs through the catalog-revision append, with no
  default backend seeded — the `planner-mtp` backend is created by
  the operator, like the Nemotron opt-in entry's workflow.

## Expected outcome

Published measurements isolate MTP at about 1.6x decode on this model
class on an RTX 3090 (n-max 2 was the best draft length, +59.8%).
Projected on the A4000: 30–37 tok/s generation, unverified until
measured here. MTP accelerates decode only; prompt processing
(prefill) is unaffected. (Under v0.1.40, draft acceptance measured
healthy at 53.9–84.0%, but the projected gain did not materialize:
the matched-workload comparison in the 2026-10-18 diagnostics
bundle shows a net decode loss from per-iteration draft overhead —
see "Shipped status and measured outcome".)

## Plan

1. **Verify the current file.** Confirm the planner GGUF is
   11,771,546,784 bytes and that a tensor listing (for example
   `gguf-dump`) shows no `blk.64.*` tensors. This confirms the base
   build and that the baseline was measured without MTP.
2. **Enable and download Planner-MTP.** In LLMdil's Models view,
   enable the `qwen3.8-27b-mtp` entry and download it. The download
   verifies against the catalog pin above; the file lands alongside
   the base planner file, which stays untouched.
3. **Create the `planner-mtp` backend.** In LLMdil, create a backend
   with id `planner-mtp` serving the Planner-MTP model:
   - Placement: A4000 only, no tensor split — the model is fully
     GPU-resident, like the planner.
   - Context and batch: copy the planner backend's values.
   - Port: any free port (backends each carry their own port; it is
     only used when measuring direct, step 4).
   - Raw flags: `--parallel 1 --spec-type draft-mtp
     --spec-draft-n-max 2` to start. (As shipped, the two
     speculative flags are set by the MTP slider instead — see
     "Shipped status" above; only `--parallel 1` still requires
     the hand edit this step describes.) Two codebase facts shape this:
     a backend's parallel-slot count exists only in raw flags (the
     structured flags generate `-c`, `-b`, `--tensor-split`, and
     `--split-mode` only — nothing emits `--parallel`, and
     llama-server's own default is 4), and `draft-mtp` is active
     only with a single slot, so `--parallel 1` must be in the raw
     flags next to the speculative flags.
   - Gateway (optional, for gateway measurements): gateway model
     names are gateway *group* names — a backend gets no gateway
     name by itself. Create a gateway group named `planner-mtp`
     with the single member `planner-mtp` (the default groups use
     the same name-per-backend shape). Without this group,
     `planner-mtp` is reachable only direct at the backend's port.
4. **A/B with the baseline script, one backend at a time.** Stop the
   planner backend before starting `planner-mtp`, and vice versa:
   the two are never run together. Their weights alone are
   10.96 GiB + 11.29 GiB ≈ 22.3 GiB against the A4000's 16 GiB,
   before any KV cache. Arms (exact invocations in
   `tools/prometheus/README.md`):
   - `planner` backend as it is (label `base-file`) — the base arm,
     re-measured.
   - `planner-mtp` with raw flags `--parallel 1` only, no
     speculative flags (label `mtp-nospec`) — isolates any cost of
     the file itself; expected to match `base-file`.
   - `planner-mtp` with `--spec-type draft-mtp` added and
     `--spec-draft-n-max` 2, then 3, then 5 (labels `mtp-nmax2`,
     `mtp-nmax3`, `mtp-nmax5`) — edit only the `planner-mtp`
     backend's raw flags between runs and restart that backend.

   Each arm is run twice: a deterministic pass (`-Deterministic`,
   temperature 0, fixed seed) and a production-sampling pass
   (default). Record median generation tok/s per prompt, plus draft
   acceptance (accepted / drafted) whenever the server reports it.
5. **Adoption gate.** Adopt MTP only if the production-sampling pass
   shows a median generation gain of at least about 1.3x over
   `base-file`. (Status under v0.1.40: not adopted. The gate's own
   script run has not been executed, but the matched-workload
   evidence in "Shipped status and measured outcome" above settles
   the configuration as run (`n_slots = 4`) well under the gate:
   spec-on generation is a net loss from per-iteration draft
   overhead despite healthy acceptance. The one configuration that
   could reopen the question, `--parallel 1`, is untested.) Adoption means keeping the winning configuration on
   `planner-mtp` and using it as the planner (for gateway callers,
   repoint the `planner` group at the `planner-mtp` backend, or
   call the `planner-mtp` group). Retiring it means stopping and
   disabling the `planner-mtp` backend and disabling the model.
   Either way the original planner model and backend keep their
   current configuration throughout — rollback is stopping
   `planner-mtp` and starting `planner`.

## Caveats

- **Single slot only.** `draft-mtp` requires `--parallel 1`, carried
  in the `planner-mtp` backend's raw flags (see step 3). The existing
  planner backend's slot behaviour is unchanged.
- **Silent no-op traps.** The pre-rename spelling `--spec-type mtp`
  is silently ignored by current llama.cpp builds. Some community
  GGUFs carry `nextn_predict_layers` metadata while stripping the
  tensors. The only proof MTP is active is the server log's
  `draft acceptance` line (equivalently `draft_n_accepted` in the
  response timings). Flags set is not evidence.
- **Output parity.** Speculative decoding is distribution-equivalent
  to ordinary sampling in theory. In practice, a study of this model
  on an RTX 3090 found greedy output under MTP was not bit-exact
  against serial decoding over long generations. The deterministic
  pass in step 4 exists to spot gross divergence on Pete's own
  prompts, not to demand bit parity.
- **Prefill unaffected.** Long-prompt latency does not improve.
  Judge MTP on generation tok/s and draft acceptance only.

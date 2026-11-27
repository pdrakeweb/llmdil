# LLMdil Build and Release

**Status: ADOPTED.** This document is the authority for how LLMdil
changes are built, gated, merged, and released. Summaries of it live
elsewhere; where a summary and this document differ, this document wins.

See also: `docs/testing-protocol.md` for the local-vs-branch
verification tiers and the minimal-local-testing protocol.

---

## 1. Default build machine

- Prometheus (Dell Precision 7865 self-hosted GitHub runner, runner
  name PROMETHEUS) is the default build machine. Full builds and gate
  suites run on Prometheus, not on a developer VM.
- Branch gates and merged-tree gates run on the Prometheus runner via
  `prometheus-gates.yml` (its `runner` dispatch input defaults to
  `prometheus`).
- If the Prometheus runner is offline, gates fall back to
  GitHub-hosted `windows-latest` (the workflow's `runner` input).
  Local full builds never substitute — the job waits or reports the
  blocker.
- Local work is quick checks only: a single-crate `cargo check` or
  one targeted test. No full workspace builds, full clippy, full test
  suites, or src-tauri gate runs locally by default.

## 2. src-tauri is not in the workspace

- The root `Cargo.toml` has `exclude = ["src-tauri"]`, so
  `cargo check/clippy/test --workspace` never compiles the Tauri layer.
- Before tagging any release, run `cargo check` (and clippy) inside
  `src-tauri/` directly — on Prometheus as part of the gate set.

## 3. Constrained-fix fast path

A constrained fix is a single scoped fix meeting all of these:

- No design decisions.
- No public API, schema, or dependency changes.
- No behavior change beyond the stated fix.
- Small diff in one feature area.

When in doubt, a change is NOT a constrained fix and takes the full path.

- Local verification for a constrained fix is lint only:
  - UI: `npx tsc --noEmit` in `ui/` plus the targeted smoke test
    when one covers the touched view.
  - Rust: `rustfmt --check` on changed files plus `cargo check` /
    `clippy` on the affected package only.
  - Docs: readback.
- Then the branch is pushed and the full build + gate suite runs on
  Prometheus via `gates_dispatch.py`, with merge-and-release by
  default applying on the Prometheus result.

## 4. Merge and release by default

- A dispatched implementation ends in merge + release when success
  is clear: quick local checks plus Prometheus branch gates and
  Prometheus merged-tree gates green, no scope growth, no invented
  design decisions, no behavior-affecting UNVERIFIED items.
- Escalate to the council only when implementation is unclear or
  results are questionable.
- Design docs need approval before any build.

## 5. Branch cleanup after a verified merge

- After a merge is verified on remote main, delete the feature
  branch's local working tree and its build outputs, and report the
  reclaimed space.
- Delete only when the merge is affirmatively verified on the
  remote. A local tree can hold the only copy of unpushed work, so an
  unverified tree is kept and reported, never deleted on assumption.
- Exemption: the standing release clone is never deleted under this
  rule, and neither is its build cache (`target/`,
  `src-tauri/target/`, `ui/node_modules`, `ui/dist`). Release merges
  and gates run in that clone so the cache stays warm and gates
  compile incrementally; general storage cleanups skip the clone
  entirely.

## 6. Commits and releases

- Commits are feature-scoped: messages name the feature or fix, not
  one undifferentiated dump of a long-lived local tree.
- Every release's notes state what was added and what was fixed, in
  the release itself, not only in chat.
- Every release report includes the GitHub release page URL and the
  direct installer download URL.

## 7. GitHub sync note

- The sync tool's `SKIP_DIRS` includes `.github`, so workflow files
  in a local tree are NEVER pushed. When a workflow doesn't fire,
  check the remote contents (`GET /contents/.github/workflows`)
  before debugging triggers.

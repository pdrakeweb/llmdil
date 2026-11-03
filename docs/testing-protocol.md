# LLMdil Testing Protocol

**Status: ADOPTED (Oct 22, 2026).** This document is the authority for
how LLMdil changes are verified. Summaries of it live in the workspace
AGENTS.md rule and the parallelism governor's docs; where a summary and
this document differ, this document wins.

Values that began as judgment calls in the draft were adopted as
written; §8 records them.

---

## 1. Principle

This VM (2 vCPUs, ~7.7 GiB RAM) is for editing and lightweight verification
only. Authoritative checking and testing happen on GitHub branches built
on Prometheus (the Dell Precision 7865 self-hosted runner, repo runner
name PROMETHEUS; verified online via the GitHub API on Oct 22, 2026).

Local full-workspace builds and test suites are the exception, never the
default, for **any** class of change — not just small ones. The Oct 22,
2026 failure this protocol exists to prevent: a 3-file UI fix (+98/−15)
took 2h13m wall-clock because its worker ran full-workspace Rust gates
locally, under contention with a second local build, on a nearly full
disk. The edit took minutes; the local gate weight took the rest.

Two tiers:

- **Local tier** — file-scoped lint, crate-scoped lint where file scope
  cannot reach, and fast unit tests on small crates. Minutes, not hours.
- **Branch tier** — the full build + gate suite on Prometheus via the
  gates workflow. This is where correctness is proven.

## 2. Local tier (default for all code work)

### 2.1 Determine the changed-file set first

Every local check starts from the files actually modified:

```sh
git status --porcelain            # uncommitted work
git diff --name-only main...HEAD  # committed branch work
```

Lint and test scope derive from that set — never from the whole tree by
default.

### 2.2 File-scoped lint

**Rust — format check on changed files only:**

```sh
rustfmt --edition 2021 --check <each changed .rs file>
```

(rustfmt 1.10.0 is installed under `~/.cargo/bin`. There is no repo
rustfmt.toml, so default style rules apply. The check is file-scoped by
design: do not run `cargo fmt` at workspace scope on the VM — the local
rustfmt build also reformats files the change did not touch, which
pollutes the diff.)

**UI — typecheck:** the repo has no ESLint or Prettier setup (checked
Oct 22, 2026: `ui/package.json` carries only `typescript` and `vite`).
The practical UI lint is the TypeScript check, which is project-scoped
but emits nothing and is fast:

```sh
cd ui && npx tsc --noEmit    # ~28 s wall on this VM under load; exit 0 verified Oct 22, 2026
```

Adopting a real UI linter/formatter (ESLint/Prettier) would make UI lint
file-scoped like rustfmt; that adoption is a separate decision.

**Docs / config / comments only:** no commands. Read the edited files
back and confirm the change is what was intended.

### 2.3 Crate-scoped lint (the allowed exception to file scope)

Clippy has no file scope — it operates per crate. When a Rust change
needs more than a format check, the crate-scoped variant is permitted,
for the crate(s) containing changed files **only**:

```sh
cargo check -p <crate>
cargo clippy -p <crate> --all-targets -- -D warnings
```

Honest cost note: a `-p` build still compiles that crate's dependency
graph. For a leaf crate that is minutes; it is never the workspace.
`-p` on a crate whose dependency graph is effectively the whole workspace
(e.g. the top app crate) is a full build wearing a smaller flag — it
belongs to the branch tier.

### 2.4 Fast unit tests on small crates

Running a small crate's unit tests locally is permitted when **all** of
these hold:

- **Fast:** the whole `cargo test -p <crate>` invocation — compile
  plus run, warm cache — finishes in **≤ 60 seconds** on this VM. If it
  does not, the crate's tests are branch-tier by definition, whatever
  their size.
- **Small:** the crate's test build does not pull large third-party
  crates beyond the crate's own library dependency set. Grounded
  examples (Oct 22, 2026 manifests): `manager-config` (serde, thiserror,
  chrono, zip) is small. `manager-models` already depends on reqwest and
  tokio as library deps, so its tests compile those anyway — permitted,
  subject to the 60-second bound. Anything whose tests drag in the Tauri
  or Windows stack (`src-tauri`, `manager-winml`) is not small.

**Standing test-design rule** (applies to new code and code review):
a crate's unit tests use `std` plus the crate's existing library
dependencies only. Do not add heavy dev-dependencies to make a unit test
convenient. A scenario that needs a heavy dependency, a network, a
filesystem fixture of any size, or another process is an **integration
test**, and integration tests live in the branch tier.

### 2.5 What does NOT run locally by default

- `cargo check` / `cargo clippy` / `cargo test` with `--workspace` or
  `--all-targets` at workspace scope
- Release builds (`cargo build --release`, installer builds)
- Full UI builds (`npm run build` / `vite build`) as a local gate — the
  branch tier runs the UI build; locally the typecheck in §2.2 stands in
  for it
- Any `src-tauri` gate run (its check/clippy also cannot link its test
  binary on this VM; Windows/Prometheus is where src-tauri is proven)
- The full UI smoke suite as a local gate, except the targeted case in
  §4

**Exceptions — narrow, and named:** a change that genuinely cannot be
exercised any other way before merge (e.g. a platform-specific
interaction with no branch-tier coverage). The worker states the
exception and the reason in its report; Pete decides on review, or the
dispatching chat decides at dispatch time and writes the exception into
the brief. Silence is not an exception: an unlisted local full build is
a protocol violation, and the janitor's gates-route check treats briefs
that instruct one as defective.

## 3. Branch tier (GitHub + Prometheus)

1. Push the branch. (Environment note: pushes here go through the GitHub
   API tooling; `gh_sync.py` skips `.github`, so workflow-file changes
   need the workflow-scope push path.)
2. Dispatch the gates: `~/workspace/parallelism-governor/gates_dispatch.py
   --ref <branch>` — it dispatches `prometheus-gates.yml` on the
   Prometheus runner (the workflow's `runner` input, default
   `prometheus`) and watches the run. The gate set: full workspace
   check + clippy + test, `src-tauri` check + clippy + test, UI
   install + build + smoke.
3. Read results back from the helper's JSON report: path taken
   (`prometheus` or `fallback-windows-latest`), run id/URL, per-job and
   per-step results. A green claim without the report is not evidence.
4. Merge per merge-and-release-by-default; merged-tree gates run the
   same way on the merged result before the release is tagged.

**Expanded testing scales here, not locally.** As a change's size or
risk grows, what grows is the branch-tier set: integration suites,
Windows-specific coverage (including the src-tauri test binary, which
only runs on this tier), wider matrices, longer soak/benchmark runs on
Prometheus. Local scope never grows past §2 to compensate.

## 4. Change classes and their check sets

| Class | Local tier | Branch tier |
|---|---|---|
| **Constrained fix** (criteria below) | File-scoped lint only (§2.2); crate-scoped lint (§2.3) only if a changed .rs file's crate needs it; `npm run smoke` only when a smoke case already covers the touched view | Full gates via gates_dispatch.py |
| **Standard fix / small feature** | §2.2 + §2.3 for touched crates + §2.4 unit tests where they qualify | Full gates; integration tests for the touched area |
| **Feature / multi-crate work** | §2.2 + §2.3 for touched crates; §2.4 where they qualify | Full gates + expanded testing per §3 |
| **Refactor** | Same as feature — refactors get no local discount | Full gates + expanded testing; behavior-equivalence evidence on the branch |
| **Docs / comments only** | Readback only | Gates optional at dispatcher's call; the change cannot break a build it does not touch |

### Constrained fix — classification criteria

A change is a constrained fix only if **all** hold; when in doubt, it is
not one:

1. A single scoped fix for one stated problem; no design decisions left
   open — the brief can state the exact change.
2. No public API changes: no new or changed manager ops, no Tauri
   command signature changes, no config/schema/storage format changes.
3. No dependency changes: `Cargo.toml`/`Cargo.lock`,
   `ui/package.json`/`package-lock.json` untouched.
4. No behavior change beyond the stated fix — no refactors, renames that
   ripple, or opportunistic cleanup riding along.
5. Small, contained diff: one feature area; guide ≤ 5 files and ≤ ~150
   changed lines (exemplar: the card-overflow fix, 3 files,
   +98/−15).
6. Nothing under `src-tauri/` — its gates need the Windows
   environment, and local src-tauri checks on this VM have missed errors
   that Windows CI caught (Oct 15, 2026).

Typical members: CSS/layout fixes, copy/text changes, a contained
single-crate bug fix with a known cause, doc fixes, a smoke test pinning
existing behavior. Never constrained: features, anything awaiting a
design decision, migrations, auth/security changes, build/CI/gates
changes, performance work that changes architecture, and fixes whose
root cause is not yet diagnosed.

Classification is judged on the **final diff**, not the estimate. If a
fix outgrows the criteria mid-work, the worker stops, says so in its
report, and the item converts to the standard path.

## 5. Fallbacks and edges

- **Prometheus runner offline:** gates_dispatch.py cancels a run
  Prometheus does not pick up within the pickup timeout (default
  30 min) and re-dispatches **once** on GitHub-hosted `windows-latest`
  via the workflow's `runner` input — the only fallback. Local full
  builds never substitute. Work whose gates are not release-blocking may
  queue and wait for Prometheus instead; the dispatching chat decides
  and records it.
- **Hotfixes:** a hotfix changes urgency, not tier. Local lint
  still applies, gates still run on the branch tier (expedited: dispatch
  at P2, gates first, paperwork after). There is no "too urgent for
  Prometheus" local-build exception.
- **Empty local lint set:** docs-only changes (readback) and pure config
  changes with no checker (readback + the branch tier carries the proof)
  are legitimate. An empty set is stated in the brief, not discovered
  after the fact.
- **This VM's builds that do happen** (the §2.3/§2.4 compiles) run at
  background priority per the build-priority rule: through
  `~/workspace/parallelism-governor/bgrun.sh` (nice 19, ionice idle) for
  build-class governor items.

## 6. Adoption

- **Briefs cite the protocol in one line:**
  `Testing: llmdil-testing-protocol.md — local tier only; full gates on Prometheus via gates_dispatch.py.`
  Constrained fixes add the governor flag instead:
  `governor.py enqueue --constrained` records the classification on the
  item and injects the fast-path block (marker
  `<!-- governor:constrained-fix -->`) into the stored brief, so every
  spawn path — chat dispatch and janitor ticks alike — carries it
  verbatim. The switch lives in `limits.yaml` (`constrained_fix.enabled`).
- **Classification:** the dispatching chat classifies at enqueue using
  §4; the janitor's refill scan applies the same criteria when it writes
  briefs. Misclassification is corrected by converting the item (§4),
  never by silently broadening local checks.
- **Merge-and-release-by-default is unchanged.** What changes is the
  evidence: "gates green" for most work now means the local-tier record
  (lint commands + results in the worker's report) plus the branch-tier
  gates_dispatch.py report — not a local full-gate run.
- **Composition with the Prometheus-only enforcement (Oct 22, 2026):**
  that work owns routing — gates_dispatch.py v2 has no local gate path,
  and the janitor refuses to spawn LLMdil build briefs that instruct
  local full gates. This protocol is the classification and local-check
  layer on top of that routing; the two do not overlap.

## 7. Adoption record

Adopted Oct 22, 2026 and committed to the LLMdil repo as
`docs/testing-protocol.md` (this document). The remaining adoption steps
live outside this repo and are tracked with the parallelism governor:
the governor's STATUS.md/DESIGN.md addenda and the workspace AGENTS.md
rule cite this repo copy as the authority, and the §6 citation line is
part of the standing brief boilerplate dispatchers and the janitor's
refill use.

## 8. Judgment calls adopted at approval

The draft flagged the following as judgment calls Pete had not
explicitly stated. All were adopted as written on Oct 22, 2026; the
values below are the settled protocol.

- **[JC-1]** "Fast" = ≤ 60 s for a whole `cargo test -p <crate>`
  invocation, warm cache. Pete said "fast unit tests"; the number was
  the draft author's.
- **[JC-2]** Crate-scoped clippy/check on a crate whose dependency graph
  is effectively the workspace counts as a full build and is branch-tier.
  The principle is Pete's; the boundary case was the draft author's call.
- **[JC-3]** `src-tauri/` changes are excluded from constrained fixes
  outright (grounded in the Oct 15, 2026 local-gate miss).
- **[JC-4]** Hotfixes keep the branch tier with no local-build exception
  (extends the enforcement rule to a case Pete had not addressed).
- **[JC-5]** The UI has no linter/formatter today; the protocol adopts
  `tsc --noEmit` as UI lint and defers an ESLint/Prettier adoption
  decision.
- **[JC-6]** The constrained-fix size guide (≤ 5 files, ≤ ~150 changed
  lines) is the draft author's quantification of Pete's "tens of lines,
  not hundreds".
- **[JC-7]** The §2.5 exception path (a change that cannot be exercised
  any other way) names Pete or the dispatching chat as decider; Pete had
  not specified an exception process.

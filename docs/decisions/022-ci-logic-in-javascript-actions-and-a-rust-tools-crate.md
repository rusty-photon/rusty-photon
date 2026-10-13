# ADR-022: CI logic lives in JavaScript actions or a Rust tools crate, not in shell

## Status

Accepted (2026-10-13). The migration is tracked in
[`docs/plans/ci-shell-to-code.md`](../plans/ci-shell-to-code.md). Until an
item there lands, the shell it replaces stays authoritative; new CI logic
follows this decision from now on.

## Context

The CI is mostly shell. On 2026-10-13, `main` had 210 `run:` blocks
(1,703 non-comment lines) across 20 workflows and 6 composite actions, 92
of them with real logic (more than five lines, or control flow). On top of
that, `scripts/` and `tools/ci/` held about 8,700 lines of bash and
PowerShell, not counting the runner pool's test harness.

Shell has cost this repo in four ways:

- **Copies.** The five `install-*` actions (OmniSim, Pebble, ASTAP, the
  ZWO and SVBony SDKs) each do one job — pick the asset for the OS,
  download it, check its SHA-256, unpack it, export a path — about 1,400
  lines in all, written five times over. Two of them do it twice, in bash
  and again in PowerShell for Windows. A fix lands in one copy and not
  the others.
- **No tests.** The one shell program with a test harness is the runner
  pool orchestrator (`tools/ci/tests/`). Nothing else is exercised before
  it runs in CI, and a workflow step can only be tried by pushing it.
- **Defects of the language.** Quoting, word splitting, a pipeline's exit
  status read through `!`, a `local` that shadows an outer variable:
  [code-review.md](../skills/code-review.md#what-the-record-shows) records
  five genuine bugs drawn by new bash in one PR (#857), and check.yml's
  shellcheck job comment names the orchestrator's worst defects as this
  kind.
- **No home for shared logic.** The block that opens or updates a nightly
  tracking issue exists ten times, in ten workflows.

Two constraints decide what can replace it:

1. **Some logic runs before any toolchain exists** — installing tools,
   choosing Bazel's remote-cache flags, restoring a cache. Whatever runs
   there must already be on every runner: GitHub-hosted Linux, Windows and
   macOS, the Proxmox pool's Linux and Windows templates, and the Pi.
2. **Some logic is a developer tool as much as a CI step** —
   `tools/ci/bazel-clippy.sh` is in AGENTS.md rule 4's local gate,
   `scripts/repin-bazel-lock.sh` in rule 10. It has to run on a developer's
   machine without CI around it, and it benefits from knowing the
   workspace (crates, features, pins).

## Decision

Each piece of CI logic goes to exactly one of three homes.

1. **Workflow YAML** keeps a step that runs one command — a `bazel`,
   `cargo` or similar invocation, with its flags — and has no control flow.
   Moving a one-line command into a program only hides it.

2. **JavaScript, as local actions**, for logic that runs inside GitHub
   Actions, above all before a toolchain exists or against the GitHub API:
   - one directory per action under `.github/actions/<name>/`, with
     `runs.using: node24`, the Node the runner itself ships, so nothing is
     installed first and one file serves every OS;
   - **Node built-ins only**: no npm dependencies, no `package.json`, no
     bundler or build step, no TypeScript. What a dependency would provide
     (an archive reader, request signing) is written here and tested here;
   - code shared between actions lives in one helper module inside
     `.github/actions/`;
   - an action that needs another action (`actions/cache`) is a thin
     composite wrapper around the JavaScript action, since a JavaScript
     action cannot call one;
   - tests run with `node --test` under `bazel test //...`, on a Node the
     repo pins to the same major version as `runs.using`, so they run on the
     Linux and Windows PR legs like every other test.

3. **Rust, in one tools crate**, for tools a developer also runs, and for
   logic that needs the workspace's own knowledge:
   - a workspace binary crate with one subcommand per tool, built and
     tested by Bazel and Cargo like any crate, under the workspace lints;
   - the shell entry points it replaces are deleted, not wrapped, and the
     docs that name them (AGENTS.md included) change in the same PR.

Three kinds of code are outside this decision:

- **Host provisioning.** `tools/ci/rp-runner-pool.sh`, `rp-edac-check.sh`,
  the runner-guest scripts, `scripts/setup-pi-runner.sh`,
  `scripts/setup-ci.sh` and `scripts/rig.sh` run on machines outside
  GitHub Actions (the hypervisor, as root; a fresh Pi; the field rig).
  Moving them means shipping a binary to those hosts, which is its own
  decision. The orchestrator also has its own test harness.
- **Python.** `tools/ci/*.py` and `tools/coverage/*.py` are already in a
  programming language. Whether to move them is a separate question.
- **Git hooks.** `.cargo-husky/hooks/pre-commit` stays shell; it calls the
  tools this decision moves.

## Consequences

- One shared JavaScript module replaces the five install copies and their
  PowerShell twins, and the logic can be tested before it reaches CI.
- The repo gains a Node toolchain in `MODULE.bazel`, for tests only;
  actions still run on the runner's own Node.
- JavaScript here has no formatter or linter: both would be npm
  dependencies. `node --check` and the tests are its gate; adding a linter
  is a later decision.
- A Rust tool costs a compile on first local use, where a script ran at
  once. In the Bazel legs it comes from the remote cache.
- Some tools that drive Bazel (`bazel-clippy.sh`) run inside the Bazel
  legs, and the pool's templates have no Cargo. The plan item that moves
  such a tool settles how it is launched there, and verifies it.
- `stable / shellcheck` lints less over time. It stays for the shell this
  decision leaves in place.
- Shell is no longer the default for new CI logic: a reviewer may ask for
  a new shell block of more than a few lines to move to one of the two
  homes above.

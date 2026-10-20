# ADR-022: CI logic lives in JavaScript actions or a Rust tools crate, not in shell

## Status

Accepted (2026-10-13). The migration is tracked in
[`docs/plans/ci-shell-to-code.md`](../plans/ci-shell-to-code.md). Until an
item there lands, the shell it replaces stays authoritative; new CI logic
follows this decision from now on. Amended 2026-10-18
([Amendment A](#amendment-a--2026-10-18-a-developers-install-runs-the-install-actions-module)):
a developer's install of what an `install-*` action installs is that
action's JavaScript, not Rust.

## Context

The CI is mostly shell. On 2026-10-13, `main` had 210 `run:` blocks
(1,703 non-comment lines) across 20 workflows and 6 composite actions, 92
of them with real logic (more than five lines, or control flow). On top of
that, `scripts/` and `tools/ci/` held about 8,700 lines of bash and
PowerShell, not counting the runner pool's test harness.

Shell has cost this repo in four ways:

- **Copies.** The five `install-*` actions (OmniSim, Pebble, ASTAP, the
  ZWO and SVBony SDKs) each do the same kind of job — choose what to fetch
  for the OS, download it with retries, check it against a pin, unpack or
  stage it, export paths — in about 1,400 lines, written five times over.
  The pins differ (a SHA-256, a git ref, a rolling vendor URL), the
  retry and unpack code does not. Two of them do it twice, in bash and
  again in PowerShell for Windows. A fix lands in one copy and not the
  others.
- **No tests.** The only shell with a test harness is the runner pool's
  host tooling, the orchestrator and the EDAC check (`tools/ci/tests/`).
  Nothing else is exercised before it runs in CI, and a workflow step can
  only be tried by pushing it.
- **Defects of the language.** Quoting, word splitting, a pipeline's exit
  status read through `!`, a `local` that shadows an outer variable:
  [code-review.md](../skills/code-review.md#what-the-record-shows) records
  five genuine bugs drawn by new bash in one PR (#857), and check.yml's
  shellcheck job comment names the orchestrator's worst defects as this
  kind.
- **No home for shared logic.** Even the CI's existing JavaScript is
  pasted inline: the `actions/github-script` block that opens or updates a
  nightly tracking issue exists ten times, in ten workflows, because there
  is nowhere to put it once.

Two constraints decide what can replace it:

1. **Some logic runs before any toolchain exists** — installing tools,
   choosing Bazel's remote-cache flags, restoring a cache. Whatever runs
   there must already be on every runner: GitHub-hosted Linux, Windows and
   macOS, the Proxmox pool's Linux and Windows templates, and the Pi.
2. **Some logic is a developer tool as much as a CI step** —
   `tools/ci/bazel-clippy.sh` was in AGENTS.md rule 4's local gate (until
   the plan's R2 removed Bazel's clippy), the lock repin is in rule 10. It
   has to run on a developer's machine without CI around it, and it
   benefits from knowing the workspace (crates, features, pins).

## Decision

Each piece of CI logic goes to exactly one of three homes.

1. **Workflow YAML** keeps a step that runs one command — a `bazel`,
   `cargo` or similar invocation, with its flags — or a short fixed
   sequence of them, with no control flow. Moving a few plain commands
   into a program only hides them.

2. **JavaScript, as local actions**, for logic that runs inside GitHub
   Actions, above all before a toolchain exists or against the GitHub API:
   - one directory per action under `.github/actions/<name>/`, with
     `runs.using: node24`, the Node the runner itself ships, so nothing is
     installed first and one file serves every OS;
   - **Node built-ins only**: no npm dependencies, no `package.json`, no
     bundler or build step, no TypeScript. What a dependency would provide
     (an archive reader, request signing) is written here and tested here;
   - code shared between actions lives in one helper module, placed
     where Bazel can test it (see Consequences);
   - inline `actions/github-script` follows the same rule as shell: a few
     lines may stay in the workflow, and anything longer, or repeated,
     becomes a local action;
   - an action that needs another action (`actions/cache`) is a thin
     composite wrapper, since a JavaScript action cannot call one. The
     JavaScript runs first to compute what the other action needs (a
     cache key that carries the pin), then the other action runs, then
     the JavaScript runs again with its result (whether the cache hit);
   - tests run with `node --test` under `bazel test //...`, on a Node the
     repo pins to the same major version as `runs.using`, so they run on the
     Linux and Windows PR legs like every other test.

3. **Rust, in one tools crate**, for tools a developer also runs:
   - a workspace binary crate with one subcommand per tool, built and
     tested by Bazel and Cargo like any crate, under the workspace lints;
   - the shell entry points it replaces are deleted, not wrapped, and the
     docs that name them (AGENTS.md included) change in the same PR.

**When both homes fit, who runs it decides.** Logic that only CI runs is
JavaScript, even when it reads the workspace: `cargo metadata` and
`bazel query` print JSON and text that JavaScript reads as well as Rust
does, and a CI job then compiles nothing first. A tool a developer also
runs is Rust, except a developer's install of what an `install-*` action
installs (Amendment A).

Three kinds of code are outside this decision:

- **Host provisioning.** `tools/ci/rp-runner-pool.sh`, `rp-edac-check.sh`,
  the runner-guest scripts, `scripts/setup-pi-runner.sh`,
  `scripts/setup-ci.sh` and `scripts/rig.sh` run on machines outside
  GitHub Actions (the hypervisor, as root; a fresh Pi; the field rig).
  Moving them means shipping a binary to those hosts, which is its own
  decision. The orchestrator also has its own test harness.
- **Python.** `tools/ci/*.py` and `tools/coverage/*.py` are already in a
  programming language. Whether to move them is a separate question.
- **Git hooks.** `.cargo-husky/hooks/pre-commit` stays shell. It runs
  Cargo, Bazel and the Python lock check, and names the repin tool only
  in a comment; the lock check's messages are what send developers to
  it.

## Consequences

- One shared JavaScript module replaces the five install copies and their
  PowerShell twins, and the logic can be tested before it reaches CI.
- The repo gains a Node toolchain in `MODULE.bazel`, for tests and, since
  Amendment A, a developer's install; actions still run on the runner's
  own Node.
- `.github` had been in `.bazelignore` since Bazel arrived (#82). The
  plan's F1 took it out: the helper module and every action's tests live
  under `.github/actions`, in one Bazel package
  ([ci-logic.md](../skills/ci-logic.md)).
- JavaScript here has no formatter or linter: both would be npm
  dependencies. Its gate is the tests and a Bazel test that parses and
  links every action file without running it, so a syntax error or a
  wrong import in a file no test imports still fails the PR; adding a
  linter is a later decision.
- A Rust tool costs a compile on first local use, where a script ran at
  once. In the Bazel legs it comes from the remote cache.
- A tool that drives Bazel from inside the Bazel legs would meet a job
  where Cargo may not be on the PATH: the Linux pool template has none
  until the plan's T1 adds it, and the Windows template has not been
  checked. The one such tool, `bazel-clippy.sh`, went with Bazel's clippy
  instead (the plan's R2), and its checks of the toolchain pins became a
  test, so no tool has had to settle this yet. One that does settles how it
  is launched there, and verifies it.
- `stable / shellcheck` lints less over time. It stays for the shell this
  decision leaves in place.
- Shell is no longer the default for new CI logic: a reviewer may ask for
  a new shell block of more than a few lines to move to one of the two
  homes above.

## Amendments

### Amendment A — 2026-10-18: a developer's install runs the install action's module

Plan item J3 had to decide what replaces `scripts/install-pebble.sh`, the
one developer installer the repository had, which kept a second copy of
`install-pebble`'s pin table. The Decision above would make it a Rust
tool. Its siblings showed the cost: OmniSim had no developer installer at
all, and the ZWO SDK's README carried a copy-paste block with a second copy
of its action's pin. A Rust installer for each would reimplement, in a second
language, what `.github/actions/lib/install.mjs` already does: choose the
asset, download it, check its pin, unpack it.

**Decision.** A developer installs what an `install-*` action installs
with `bazel run //.github/actions:install -- <tool>`, which runs that
action's own module on the Node `MODULE.bazel` pins (`node_binary`, beside
`node_test`). The pins, the download and the unpacking stay in one place
and one language, tested once. Nothing else moves: a developer tool that
is not an install stays Rust.

**What it may install.** It downloads on the developer's machine, from
where CI downloads, the posture ADR-013 and ADR-018 take for the operator
helpers; it ships nothing, and nothing is mirrored for developers. A tool
whose CI download comes from the project's private mirror (ADR-018 §7,
SVBony's Windows SDK) is not offered to developers.

**Consequences.** `bazel run` works only from a checkout with Bazel, which
every developer of this repository has (AGENTS.md rule 4's gate runs it).
J3 brings OmniSim and Pebble, and ASTAP followed J4; J5 and J6 add the
ZWO and SVBony SDKs' public downloads, retiring the READMEs' copy-paste
blocks.

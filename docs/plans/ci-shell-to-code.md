# CI shell to JavaScript and Rust

## Goal

Move the CI's logic out of shell, into the two homes
[ADR-022](../decisions/022-ci-logic-in-javascript-actions-and-a-rust-tools-crate.md)
chose: **local JavaScript actions** for logic that runs inside GitHub
Actions, and **one Rust tools crate** for tools a developer also runs. A
step that runs one command stays in the workflow YAML.

The plan is a menu, not a sequence. Each item below is one pull request
that can be picked up on its own once the items in its **Needs** column
have merged. Two foundation items, F1 (JavaScript) and F2 (Rust), come
first in their columns.

## Working an item

- **One item, one PR**, branched from `main`, with the item's ID in the
  title. The PR sets the item's status in the table below.
- **The shell goes in the same PR.** An item deletes what it replaces; no
  item leaves two copies of the same logic running.
- **Prove it where it runs.** A PR's own CI only exercises the workflows
  its events trigger. For logic that runs in a nightly, scheduled or
  release workflow, dispatch that workflow on the branch
  (`gh workflow run <file> --ref <branch>`) and link the run in the PR.
  List every venue the code runs on — hosted Linux, Windows and macOS, the
  pool's Linux and Windows templates, the Pi — and show each one green.
- **Tests come with the code.** JavaScript is tested with `node --test`
  under Bazel (F1), Rust like any crate. A test that guards a behaviour is
  shown to fail when the behaviour is broken (docs/skills/testing.md).
- **Docs move with it.** A PR that renames a command updates every doc
  that names it, AGENTS.md included (rule 2).

## Items

| ID  | Item | Replaces | Needs | Status |
| --- | ---- | -------- | ----- | ------ |
| F1  | JavaScript foundation: helper module, Node test rule, conventions | — | — | Not started |
| F2  | Rust tools crate, with its first tool (`repin-bazel-lock`) | `scripts/repin-bazel-lock.sh` | — | Not started |
| J1  | Cargo cache on the LAN object store | Swatinem/rust-cache on the pool `stable / clippy` | F1, #1505 | Not started |
| J2  | Shared download-verify-extract module, first used by `install-omnisim` | `install-omnisim` | F1 | Not started |
| J3  | `install-pebble` on the module | `install-pebble` | J2 | Not started |
| J4  | `install-astap` on the module | `install-astap` | J2 | Not started |
| J5  | `install-zwo-sdk` on the module | `install-zwo-sdk` (bash + PowerShell) | J2 | Not started |
| J6  | `install-svbony-sdk` on the module | `install-svbony-sdk` (bash + PowerShell) | J2 | Not started |
| J7  | Nightly tracking-issue action | 10 `github-script` copies | F1 | Not started |
| J8  | ASTAP pin refresh | `refresh-astap-shas.yml`'s bash | F1, J4 | Not started |
| J9  | Release and nightly publishing glue | `nightly-packages.yml` / `release.yml` logic blocks | F1 | Not started |
| J10 | Bazel remote-cache flags | `tools/ci/bazel-remote-flags.sh` | F1 | Not started |
| R1  | Toolchain checksums | `scripts/rust-toolchain-sha256s.sh` | F2 | Not started |
| R2  | Bazel clippy | `tools/ci/bazel-clippy.sh` | F2 | Not started |
| R3  | Bazel/Cargo target parity | `scripts/check-bazel-cargo-parity.sh` | F2 | Not started |
| R4  | Sharded BDD runner | `bdd-sharded`'s bash | F2 | Not started |
| R5  | Workspace discovery and the MSRV check | three discovery/check blocks | F2 | Not started |
| R6  | Pool routing copies stay identical | nothing (a new guard) | F2 | Not started |
| R7  | ConformU runner | `scripts/test-conformance.sh` | F2 | Not started |
| R8  | Packaging scripts, one PR each | 12 scripts in `scripts/` | F2 | Not started |
| S1  | Sweep of the remaining inline blocks | the rest | F1 or F2 | Not started |

A suggested order, if nothing else decides it: F1, then J1 (the cache that
started this) and J7 (ten copies, small); F2 with R6 (small, and it closes
a gap on a security boundary); then J2–J6 and R2. R8 is the largest and
goes last.

## Foundations

### F1 — JavaScript foundation

- **A pinned Node for tests.** Add rules_nodejs from the Bazel Central
  Registry for a Node toolchain of the same major version as the actions'
  `runs.using: node24`.
- **A test rule** that runs `node --test` over an action's test files, as
  an ordinary Bazel test, on Linux, Windows and macOS. On Windows the
  launcher cannot be a shell script.
- **The helper module** in `.github/actions/`, which every action imports:
  reading inputs (`INPUT_*`); writing to the files `GITHUB_OUTPUT`,
  `GITHUB_ENV` and `GITHUB_PATH` name (multi-line values in the delimiter
  form); masking (`::add-mask::`); log groups; failing the step; HTTP with
  timeouts and bounded retries over `fetch`; SHA-256 of a stream.
- **Conventions** in a new skill doc that F2 extends, linked from AGENTS.md
  rule 1: where actions live, Node built-ins only, how to test, how to
  wrap an action that needs `actions/cache`.
- **Done when** a test of the helper module runs in `bazel test //...` on
  the Linux and Windows PR legs, and a deliberately broken helper turns
  both legs red.

### F2 — Rust tools crate

- A workspace binary crate, proposed name `rusty-photon-ci`, with one
  subcommand per tool, `publish = false`, under the workspace lints and
  MSRV, built and tested by both Bazel and Cargo (it joins the parity
  check like any crate).
- A Cargo alias (`cargo ci <tool>`) in `.cargo/config.toml`, so developers
  have a short command that needs nothing but Cargo.
- **First tool:** `repin-bazel-lock`, replacing
  `scripts/repin-bazel-lock.sh` (60 lines): it refreshes and stages
  `MODULE.bazel.lock`, and AGENTS.md rule 10 and the pre-commit hook send
  developers to it. It touches no CI step, so it proves the crate without
  risking one. The hook stays shell (ADR-022) and names the new command.
- **Done when** the shell script is gone, rule 10, pre-push.md and the
  hook name the new command, and its report of unstaged Cargo files is
  unit-tested.

## JavaScript items

### J1 — Cargo cache on the LAN object store

The `stable / clippy` job moved to the pool in #1505, where
Swatinem/rust-cache cannot help: its cache lives in GitHub, across the
operator's WAN. This item gives that job a cache on the LAN.

- **Store:** an S3-compatible object store on the operator's NAS, serving
  anonymous reads of one bucket over plain HTTP and accepting writes
  signed with AWS Signature Version 4. The spike below used Garage.
- **The action:** `.github/actions/cargo-cache/`, `runs.using: node24`.
  Its main step restores; its `post:` step saves. No endpoint means it
  does nothing, which covers hosted runners and fork PRs.
- **Endpoint:** reaches the job from the runner, never from a workflow
  file, and is masked before use, as the bazel-remote endpoint is
  ([proxmox-runner-pool.md](../skills/proxmox-runner-pool.md)). Either a
  new runner `.env` entry (a template rebuild) or derived from
  `RP_LAN_CACHE_URL`'s host, as the Remote Asset API's endpoint is.
  Decide in the item.
- **Write key:** a GitHub secret attached only on push to main, the way
  bazel.yml attaches `BAZEL_LAN_CACHE_WRITE_AUTH`
  (`github.event_name == 'push' && secrets.… || ''`). It never lives on
  the runner VM, so a PR can read the cache but never write it.
- **What is cached:** `target/`, with the workspace members' own outputs
  removed (every run re-lints them anyway), plus `$CARGO_HOME/registry`'s
  `index`, `cache` and `src`. Archived with the system `tar`. Whether to
  compress (system `zstd`, if present) or send it raw over the LAN is
  measured in the item; Node 24's built-in zstd is still marked
  experimental.
- **Key and retention:** one object per Rust version, overwritten by each
  push to main. Objects for older Rust versions must not pile up; decide
  how they go.
- **Signing:** SigV4 with `node:crypto`, tested against AWS's published
  Signature Version 4 examples. The unsigned-payload form lets the upload
  stream without hashing the archive first.
- **Failure:** a missing object, a slow store or a store that is down
  gives a cold run, never a failed job. Every request has a timeout.
- **Spike (2026-10-13, local Docker Garage, both clippy passes at `-j 6`,
  the same paths for both runs):** cold 113 s; after a restore, 34 s. The
  restore took 1.2 s and the upload 0.6 s, for a 326 MiB archive at
  `zstd -3`. Restoring to different paths saved nothing, because Cargo
  fingerprints absolute paths; the pool's paths are fixed.
- **Operator steps, outside the repo:** run the store, publish only its
  S3 and website ports, and only on the runner VLAN interface; create the
  bucket and allow anonymous website reads of it; create a key that can
  write that bucket; add the key as a repository secret.
- **Done when** a pool `stable / clippy` run on a PR restores the cache
  and runs warm, a push to main saves it, and the job still passes with
  the store stopped.

### J2 — Shared download-verify-extract module, first used by `install-omnisim`

- One module for what all five `install-*` actions do: choose the asset
  for the OS and architecture from a pinned table, download it with
  retries and a stall timeout, verify its SHA-256, unpack it, export paths.
- **Unpacking:** the assets are `.tar.gz`, `.tar.xz` and `.zip`. Node's
  built-ins read none of these archive formats and have no xz decoder, so
  the module runs the platform's `tar`; macOS's and Windows' `tar` read
  zip too. Linux's GNU tar does not: today's shell uses `unzip`. Decide
  between that and a small zip reader on `zlib`'s raw inflate, and test
  whichever it is.
- `install-omnisim` (179 lines, used by six workflows) becomes a composite
  wrapper: its `actions/cache` step, then the JavaScript action.
- **Done when** `install-omnisim`'s bash is gone, every workflow that uses
  it is green on each of its venues, and the module's tests cover asset
  choice, a checksum mismatch and a truncated download.

### J3–J6 — The other install actions

Each moves onto J2's module in its own PR. What each brings that J2 did
not:

- **J3 `install-pebble`** (130 lines). `scripts/install-pebble.sh` mirrors
  its pin table for developers; after this item there is one table both
  read, or the script goes.
- **J4 `install-astap`** (283 lines). The D05 star database as a second
  asset, and SourceForge mirrors: today's curl flags re-roll a stalled
  mirror (the step's comments explain the numbers), which the module's
  stall timeout must reproduce.
- **J5 `install-zwo-sdk`** (442 lines, bash and PowerShell). A sudo and a
  sudo-free Linux path, a macOS path and a Windows path; the cache restore
  and save are separate steps.
- **J6 `install-svbony-sdk`** (367 lines, bash and PowerShell). Windows
  downloads from the project's private mirror; Linux and macOS from the
  INDI packaging.

### J7 — Nightly tracking-issue action

Ten workflows (check, conformu, install-astap, native, nightly-packages,
pi-nightly, plate-solver-smoke, safety, scheduled, ui-browser-nightly)
each carry a `github-script` block that opens a tracking issue for a
failed scheduled run, or comments on the open one. One action with
`title`, `label` and `body` inputs replaces them. Those jobs check nothing
out today; a local action needs a sparse checkout of `.github/actions`
first. `refresh-astap-shas.yml`'s rotation issue and `clippy-beta`'s
per-lint census are different and stay out unless they fit without
special cases.

### J8 — ASTAP pin refresh

`refresh-astap-shas.yml` is the largest block of workflow shell (260
lines): it detects an upstream rotation per platform, checks a new binary
solves M101, rewrites the pins and opens or updates a PR. After J4 the
pins live in J4's table, which this item edits instead of rewriting
`action.yml` with text substitution.

### J9 — Release and nightly publishing glue

`nightly-packages.yml` (the version dialects, the skip when the nightly
already has this SHA, the rolling prerelease publish, the rpm asset names,
the tap push, the previous MSI fetch) and `release.yml` (the tag-version
check, the formula commit and push). Split it per workflow if the PR gets
large. Prove it with a dispatched nightly run.

### J10 — Bazel remote-cache flags

`tools/ci/bazel-remote-flags.sh` is `source`d by bazel.yml and
bazel-coverage.yml to build a `REMOTE_FLAGS` array per venue. An action
that writes the same flags to a bazelrc file the workspace `.bazelrc`
imports with `try-import` removes the sourcing from every Bazel step. The
mask step stays inline in the YAML: it runs before checkout, and a local
action cannot.

## Rust items

### R1 — Toolchain checksums

`scripts/rust-toolchain-sha256s.sh` (124 lines), which pre-push.md sends
developers to when a Rust pin moves: it prints MODULE.bazel's `sha256s`
entries from a release's channel manifest, after checking the manifest's
detached signature against the Rust release key, pinned by fingerprint.
Today `gpg` does the check. The port either keeps calling `gpg` or takes
an OpenPGP crate; decide in the item. The manifest parsing is unit-tested
against a saved manifest.

### R2 — Bazel clippy

`tools/ci/bazel-clippy.sh` (184 lines): runs the clippy aspect over
`//...` and checks the toolchain pins agree. It is in AGENTS.md rule 4's
local gate and in bazel.yml's `bazel clippy` step. **Open question:** the
pool's templates have no Cargo, so in the Bazel legs the tool cannot
start through the alias. `bazel run` would make the tool start Bazel from inside a
`bazel run`; building it first and running it from `bazel-bin` avoids
that. Verify the choice on the Linux and Windows legs; don't assume it.

### R3 — Bazel/Cargo target parity

`scripts/check-bazel-cargo-parity.sh` (82 lines), run by `parity.yml`
nightly. Prove it with a dispatched run.

### R4 — Sharded BDD runner

The `bdd-sharded` composite action (157 lines of bash) runs the BDD
suites as parallel shard processes for the Cargo safety net (`test.yml`
three times, `scheduled.yml` three times). Which suites can shard is
workspace knowledge. The action shrinks to one command.

### R5 — Workspace discovery and the MSRV check

check.yml's "Verify workspace MSRV", scheduled.yml's "Find Miri services"
and conformu.yml's "Discover ConformU services" each read the workspace
with text tools. Each becomes a subcommand that prints what its workflow
consumes (a JSON matrix, or a pass/fail).

### R6 — Pool routing copies stay identical

The pool's `runs-on` routing expression is copied into bazel.yml,
bazel-coverage.yml and repin-bazel.yml, and #1505 adds a copy to
check.yml. ADR-020 makes it a security boundary, and only a comment keeps
the copies in step. A test in the tools crate reads the workflows and
asserts that the copies for each OS agree, so `bazel test //...` catches a
widened copy on the PR that widens it. Raised by the review of #1505.

### R7 — ConformU runner

`scripts/test-conformance.sh` (450 lines), which hardware-validation.md
and pre-push.md send developers to. conformu.yml's "Run ConformU tests
(parallel)" block may fold into the same subcommand.

### R8 — Packaging scripts

About 4,500 lines in 12 files: `build-packages.sh`, `build-tarballs.sh`,
`build-apt-repo.sh`, `build-yum-repo.sh`, `push-packages-repo.sh`,
`generate-brew-formulas.sh`, `verify-packages.sh`,
`verify-packages-repo.sh`, `verify-brew.sh`, `check-pkg-assets.sh`,
`build-msi.ps1` and `verify-msi.ps1`. One PR per script, or per tight
pair (a builder and its verifier). Each is proven by the packaging
workflows that run it (`nightly-packages.yml`, `release.yml`, `msi.yml`)
before its shell is deleted. ADRs 012 to 018 and the packaging docs name
these scripts and change with them.

## S1 — Sweep of the remaining inline blocks

What is left after the items above, each moved by the rule in ADR-022 or
left in place when it is really one command. Small PRs, in any order:

- bazel-coverage.yml: "Annotate uncovered diff lines", "Capture bazel
  test logs", "Split combined lcov per package";
- plate-solver-smoke.yml: "Assert solve-time budget", "Log per-scenario
  timing";
- pi-nightly.yml: "Symlink libusb for the QHYCCD static link";
- repin-bazel.yml: "Skip if already in sync", "Commit and push repin";
- safety.yml: "Enable opt-level 1 for sanitizers";
- check.yml: shellcheck's "Install pinned shellcheck" and its script list,
  and `clippy-beta`'s report and aggregation blocks.

## Not in this plan

- **Host provisioning, Python and git hooks**, which ADR-022 leaves where
  they are.
- **`proxmox-runner-test.yml`**, the pool's own validation workflow: its
  blocks inspect the templates, and they move, if at all, with the
  orchestrator.
- **bazel.yml's Windows doc-test diagnostics**, which are temporary:
  delete them when their question is answered rather than port them.
- **The two Cloudflare Workers** (`tools/bazel-cache-worker`,
  `tools/private-mirror-worker`) are JavaScript already and have no tests.
  F1's test rule could cover them; that is a later choice.

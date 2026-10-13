# CI shell to JavaScript and Rust

## Goal

Move the CI's logic out of shell, into the two homes
[ADR-022](../decisions/022-ci-logic-in-javascript-actions-and-a-rust-tools-crate.md)
chose: **local JavaScript actions** for logic that runs inside GitHub
Actions, and **one Rust tools crate** for tools a developer also runs. A
step that runs one command, or a short fixed sequence with no control
flow, stays in the workflow YAML.

It also carries the three items that cut the pool `stable / clippy`'s WAN
traffic (J1, J14, T1), because the cache J1 adds is where this plan
started, and J1 and J14 are JavaScript actions that need F1. They add
caches rather than replace shell, so "The shell goes in the same PR" does
not apply to them, and T1 is template work with no code to test; a pool
run proves it.

The plan is a menu, not a sequence. Each item below is one pull request
that can be picked up on its own once the items in its **Needs** column
have merged. Two foundation items, F1 (JavaScript) and F2 (Rust), come
first in their columns.

## Working an item

- **One item, one PR**, branched from `main`, with the item's ID in the
  title. The PR sets the item's status in the table below.
- **The shell goes in the same PR.** An item deletes what it replaces; no
  item leaves two copies of the same logic running.
- **Prove it where it runs, and say what the PR cannot prove.** A PR's
  own CI only exercises the workflows its events trigger. List every venue
  the code runs on — hosted Linux, Windows and macOS, the pool's Linux and
  Windows templates, the Pi — and show each one green. For code in a
  nightly or scheduled workflow, dispatch it on the branch
  (`gh workflow run <file> --ref <branch>`) and link the run, **except**:
  - a workflow that publishes or pushes is never dispatched on a branch
    unless a dry-run input stops it first. `nightly-packages.yml` has one:
    with `dry_run=true` every job but `publish` runs (`plan`, `linux`,
    `msi`, `macos`, `repo`). `release.yml` runs only on a `v*` tag. Every
    job in `refresh-astap-shas.yml` checks out `main`, so a branch
    dispatch runs main's files, and it opens a real PR;
  - **the Pi cannot run a branch.** `pi-nightly.yml`'s job is gated on
    `github.ref == 'refs/heads/main'` and checks out `main`, so a dispatch
    on a branch skips it. An item that changes code the Pi runs (the
    install actions) proves the arm64 Linux path with the module's tests
    for that platform and watches the first nightly after merge;
  - **nor can bazel.yml's macOS leg.** bazel.yml has no
    `workflow_dispatch`, and macOS joins its matrix only on push to main
    and the schedule. The install actions also reach hosted macOS from a
    dispatched `test.yml` (OmniSim, Pebble) or `native.yml` (the ZWO SDK),
    so J2, J3 and J5 prove macOS there. What only that leg runs (J10's
    remote flags, R2's clippy) is proven by tests. Every item that changes
    code the leg runs watches its first push-to-main run. Adding
    `workflow_dispatch` to bazel.yml would change the matrix and routing
    expressions ADR-020 reviews, so an item does that only with the
    owner's say;
  - code that runs only on an event a PR cannot produce — a failed
    scheduled run, a push to main — never runs from a dispatch.

  There, the item's tests carry the proof (the logic against a fake
  GitHub API, a fake store, fixture files), the PR names what stays
  unproven until it first runs on main, and someone watches that first
  run. An item may add the dry-run path it needs as its own first PR.
- **Tests come with the code.** JavaScript is tested with `node --test`
  under Bazel (F1), Rust like any crate. A test that guards a behaviour is
  shown to fail when the behaviour is broken (docs/skills/testing.md).
- **Every mention moves with it.** A PR that renames or deletes a script
  greps the whole repo for its path, not only the docs: scripts' error
  messages, comments in `MODULE.bazel` and `.bazelrc`, other workflows,
  ADRs and AGENTS.md (rule 2).
- **Find every reader and writer of what moves.** Pins and URLs are
  parsed and rewritten in place by other files:
  - `refresh-astap-shas.yml` reads install-astap's SHA table from its
    `action.yml` ("Extract current SHA pin") and rewrites it there
    ("Patch action.yml with new SHAs");
  - `scripts/check-pkg-assets.sh` reads the ZWO and SVBony `ref`
    defaults and the ZWO Windows URLs from their `action.yml`, to check
    the packages link what CI links;
  - `tools/ci/bazel-clippy.sh` keeps its own copy of the archive list
    `scripts/rust-toolchain-sha256s.sh` prints, and its errors tell
    developers to run that script.

  An item that moves a pin updates its readers and writers in the same
  PR, or leaves the pin where they find it.

## Items

| ID  | Item | Replaces | Needs | Status |
| --- | ---- | -------- | ----- | ------ |
| F1  | JavaScript foundation: helper module, Node test rule, conventions | — | — | Not started |
| F2  | Rust tools crate, with its first tool (`repin-bazel-lock`) | `scripts/repin-bazel-lock.sh` | — | Not started |
| J1  | Cargo cache on the LAN object store | nothing (adds a cache to the pool `stable / clippy`; rust-cache stays for its hosted runs) | F1 | Not started |
| J2  | Shared download-verify-extract module, first used by `install-omnisim` | `install-omnisim` | F1 | Not started |
| J3  | `install-pebble` on the module | `install-pebble` | J2 | Not started |
| J4  | `install-astap` on the module | `install-astap` | J2 | Not started |
| J5  | `install-zwo-sdk` on the module | `install-zwo-sdk` (bash + PowerShell) | J2 | Not started |
| J6  | `install-svbony-sdk` on the module | `install-svbony-sdk` (bash + PowerShell) | J2 | Not started |
| J7  | Nightly tracking-issue action | 10 `github-script` copies | F1 | Not started |
| J8  | ASTAP pin refresh | `refresh-astap-shas.yml`'s bash | F1, J4 | Not started |
| J9  | Release and nightly publishing glue | `nightly-packages.yml` / `release.yml` logic blocks | F1 | Not started |
| J10 | Bazel remote-cache flags | `tools/ci/bazel-remote-flags.sh` | F1 | Not started |
| J11 | Bazel/Cargo target parity | `scripts/check-bazel-cargo-parity.sh` | F1 | Not started |
| J12 | Sharded BDD runner | `bdd-sharded`'s bash | F1 | Not started |
| J13 | Package-repository scripts | `build-apt-repo.sh`, `build-yum-repo.sh`, `verify-packages-repo.sh`, `push-packages-repo.sh` | F1 | Not started |
| J14 | Crates from the LAN bazel-remote | nothing (the pool `stable / clippy` stops downloading crates from crates.io) | F1 | Not started |
| R1  | Toolchain checksums | `scripts/rust-toolchain-sha256s.sh` | F2 | Not started |
| R2  | Bazel clippy | `tools/ci/bazel-clippy.sh` | F2, R1 | Not started |
| R3  | Bazel/Cargo target parity | — | — | Moved in review: now J11 |
| R4  | Sharded BDD runner | — | — | Moved in review: now J12 |
| R5  | Workspace discovery and the MSRV check | — | — | Moved in review: now in S1, as JavaScript |
| R6  | Pool routing copies stay identical | nothing (a new guard) | F1, F2 | Not started |
| R7  | ConformU runner | `scripts/test-conformance.sh` | F2 | Not started |
| R8  | Packaging scripts a developer runs, one PR each | 8 scripts in `scripts/` | F2 | Not started |
| S1  | Sweep of the remaining inline blocks | the rest (listed in S1) | F1 or F2 | Not started |
| T1  | The Linux pool template carries rustup and stable | nothing (the toolchain step stops downloading) | — | Not started |

A suggested order, if nothing else decides it: F1, then J1 (the cache that
started this), J14 and J7 (ten copies, small); F2, then R6 (small, and it
closes a gap on a security boundary); then J2–J6, J12, and R1 before R2.
R8 and J13 are the largest and go last. T1 is template work and fits
any template rebuild.

## Foundations

### F1 — JavaScript foundation

- **A pinned Node for tests.** Add rules_nodejs from the Bazel Central
  Registry for a Node toolchain of the same major version as the actions'
  `runs.using: node24`.
- **A test rule** that runs `node --test` over an action's test files, as
  an ordinary Bazel test, on Linux, Windows and macOS. On Windows the
  launcher cannot be a shell script.
- **Where the tests live.** `.github` is in `.bazelignore` (since #82, with
  no reason recorded), so a BUILD file under `.github/actions` is never
  loaded, and no Bazel test can read a workflow file. R6 must read the
  workflows, so `.github` has to leave the ignore list whatever else is
  decided; the plain answer is that F1 takes it out, checking that
  nothing else under it then reaches `//...`, and the helper and its
  tests live beside the actions. If F1 still keeps the helper elsewhere,
  it records where in ADR-022's "placed where Bazel can test it", and
  J7's sparse checkout must include that place.
- **The helper module**, which every action imports:
  reading inputs (`INPUT_*`); writing to the files `GITHUB_OUTPUT`,
  `GITHUB_ENV` and `GITHUB_PATH` name (multi-line values in the delimiter
  form); masking (`::add-mask::`); log groups; failing the step; HTTP with
  timeouts and bounded retries over `fetch`; SHA-256 of a stream.
- **A syntax check of every action file.** Tests catch a syntax error only
  in a file they import, and an action's entry file is usually not
  imported, because it runs on load. A broken entry file would then pass
  every PR check and fail only where the action runs, which for J7 is
  after a scheduled failure and for J1's save after a push to main. F1
  adds a Bazel test that runs `node --check` over every `.js` file under
  the actions and the helper, so an action added later is covered too.
- **Conventions** in a new skill doc that F2 extends, linked from AGENTS.md
  rule 1: where actions live, Node built-ins only, how to test, how to
  wrap an action that needs `actions/cache` (the order ADR-022 gives:
  resolve, then the cache, then install).
- **Done when** a test of the helper module runs in `bazel test //...` on
  the Linux and Windows PR legs, and a deliberately broken helper turns
  both legs red, as does a syntax error in an entry file no test imports.

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
  risking one. `repin-bazel.yml`'s "Repin crate index" step runs the same
  sequence on dependabot PRs, on the Linux pool, which has no Cargo until
  T1. That step is
  a fixed sequence of three commands with no control flow, which ADR-022
  leaves in YAML; its "keep the two in step" comment points at the new
  tool.
- **What names it today:** about twenty files. The one that matters most
  is `tools/ci/check_bazel_lock.py`, whose refusal messages are what a
  developer actually sees when the pre-commit hook rejects a stale lock.
  The others include AGENTS.md rule 10, the hook's comment, `.bazelrc`,
  `MODULE.bazel`, `repin-bazel.yml`, several skill, service and crate
  docs, ADR-010, open plans and the vendored crates' RELEASING.md files.
  Grep before deleting.
- **Done when** the shell script is gone, nothing in the repo names it,
  and the tool's report of unstaged Cargo files is unit-tested.

## JavaScript items

### J1 — Cargo cache on the LAN object store

#1505 moved the `stable / clippy` job to the pool, where
Swatinem/rust-cache cannot help: its cache lives in GitHub, across the
operator's WAN, so the job runs it on hosted runners only. This item
gives the pool runs a cache on the LAN; the hosted runs keep rust-cache.

- **Scope: the compile.** On the pool the job's cold clippy passes take
  about 110 s of its 2m13s. Installing the toolchain takes about 10 s more,
  over the WAN, because the pool template has no rustup; T1 puts it in the
  template. Downloading the crates is J14's.
- **Store:** an S3-compatible object store on the operator's NAS, serving
  anonymous reads of one bucket over plain HTTP and accepting writes
  signed with AWS Signature Version 4. The owner chose this on 2026-10-13,
  and the spike below used Garage.
- **An alternative to weigh first:** the LAN bazel-remote already splits
  anonymous reads from authenticated writes, and its HTTP CAS is content
  addressed, so a restore would be checked for free, with no new store,
  secret or SigV4 code. The catch is the pointer from a Rust version to
  the newest archive's digest: it needs an AC entry, which bazel-remote
  validates as a Bazel `ActionResult` unless that validation is turned
  off. Record the comparison in the PR; changing the store needs the
  owner's say.
- **The action:** `.github/actions/cargo-cache/`, `runs.using: node24`.
  Its main step restores; its `post:` step saves, with `post-if:
  success()`: a post step otherwise runs after a failed or cancelled job
  too, and would overwrite the one good archive with a half-built one. No
  endpoint means it does nothing, which covers hosted runners and fork
  PRs.
- **Endpoint:** reaches the job from the runner, never from a workflow
  file, and is masked before use, as the bazel-remote endpoint is
  ([proxmox-runner-pool.md](../skills/proxmox-runner-pool.md)). Either a
  new runner `.env` entry (a template rebuild) or derived from
  `RP_LAN_CACHE_URL`'s host, as the Remote Asset API's endpoint is.
  Decide in the item.
- **Write key:** a GitHub secret, passed only on push to main with the
  expression bazel.yml uses for `BAZEL_LAN_CACHE_WRITE_AUTH`
  (`github.event_name == 'push' && secrets.… || ''`). It never lives on
  the runner VM, so a PR can read the cache but never write it. Unlike
  bazel.yml's, it goes to the action as an input (`with:`), never into
  the job's `env:`, so no build step's environment carries it. That is
  all an input does. The save is a `post:` step that runs after the cold
  build, from the action's script and the helper in the checkout, which
  a compromised build script, running as the same user, can rewrite
  first. So a dependency compromised on main can take the key, and write
  any object the key can write. It can also poison `target/`, which the
  save uploads for every PR to restore; that much is no worse than the
  dependency itself, which those PRs compile anyway, and the next push to
  main replaces the archive with a cold build. To keep the key out of the
  build's reach, a job that builds nothing could hold it and hand the
  build job a short-lived pre-signed URL for the one object instead; the
  item weighs that against signing over the payload (below), which a URL
  minted before the archive exists cannot do.
- **Writers never restore.** A push-to-main run builds cold and saves; only
  PR runs restore. The archive holds build-script and proc-macro binaries
  that the job runs, and an anonymous plain-HTTP read has no integrity
  check, so a tampered restore must never run in a job that holds the
  write key, or be saved back for every later PR. A cold build on main
  also means the archive holds only what the current lockfile needs.
- **What is cached:** `target/`, with the workspace members' own outputs
  removed (every run re-lints them anyway), plus `$CARGO_HOME/registry`'s
  `index` and `cache` (the `.crate` files, until J14 serves those) and
  `$CARGO_HOME/git`, for the workspace's git dependency. Not
  `registry/src`: Cargo recreates it from the `.crate` files, which
  rust-cache's README gives as quicker than caching it, and it is the
  source Cargo compiles, so a restored copy would bypass the check J14
  makes of each `.crate` file. Archived with the system `tar`. Whether to
  compress (system `zstd`, if present) or send it raw over the LAN is
  measured in the item; Node 24's built-in zstd is still marked
  experimental.
- **Key and retention:** one object per Rust version and pool template,
  overwritten by each push to main from its cold build, so it never
  accumulates the dependencies of older lockfiles. The template is in the
  key because `target/` holds build-script outputs (pkg-config probes,
  bindgen output) that Cargo does not rebuild when the template's system
  libraries change; the item finds an identity the runner can read, such
  as a version in its `.env`. Objects for older Rust versions and
  templates must not pile up either; decide how they go.
- **Order between writers:** two pushes to main close together run
  concurrently, and the older commit can finish last. The save first
  checks that the run's commit is still the head of `main`, and skips
  if `main` has moved on.
- **Signing:** SigV4 with `node:crypto`, tested against AWS's published
  Signature Version 4 examples, and **over the payload**: the request
  carries the archive's SHA-256, so the store can refuse a body changed on
  the way; confirm it does, by sending a deliberately wrong hash. The
  unsigned-payload form, which the spike used, would let
  anyone who can tamper with the plain-HTTP read also replace the upload,
  undoing what "writers never restore" protects. Hashing a few hundred MB
  first costs about a second.
- **Failure:** a missing object, a slow store or a store that is down
  gives a cold run, never a failed job. Every request has a timeout. A
  failed save is an annotation on the run, so a broken save path is
  visible and not just a cache that silently stays cold.
- **Spike (2026-10-13, local Docker Garage, both clippy passes at `-j 6`,
  the same paths for both runs):** cold 113 s; after a restore, 34 s. The
  restore took 1.2 s and the upload 0.6 s, for a 326 MiB archive at
  `zstd -3`. Restoring to different paths saved nothing, because Cargo
  fingerprints absolute paths; the pool's paths are fixed.
- **Operator steps, outside the repo:** run the store, publish only its
  S3 and website ports, and only on the runner VLAN interface; create the
  bucket and allow anonymous website reads of it; create a key that can
  write that bucket; add the key as a repository secret.
- **What the PR cannot prove:** the save runs only on a push to main,
  since only that event gets the key. Before merge, run the action's save
  path by hand against the store with the key, and its tests against a
  fake store; after merge, watch the first push to main write the object
  and the next PR restore it.
- **Done when** a pool `stable / clippy` run on a PR restores the cache
  and runs warm, a push to main builds cold and saves it, and the job
  still passes with the store stopped.

### J2 — Shared download-verify-extract module, first used by `install-omnisim`

- One module for what all five `install-*` actions do: choose what to
  fetch for the OS and architecture, download it with retries and a stall
  timeout, check it against its pin, unpack or stage it, export paths.
- **Three kinds of pin**, which the module must all carry from the start:
  a SHA-256 per asset (OmniSim, Pebble, ASTAP, SVBony on Windows); a git
  ref, for the ZWO and SVBony blobs fetched from the INDI repository on
  Linux and macOS, which have no hash of their own; and none at all for
  ZWO on Windows, whose vendor URLs are rolling "latest" links cached per
  ISO week, with the previous week's download as the fallback when the
  vendor CDN fails.
- **Unpacking:** the assets are `.tar.gz`, `.tar.xz` and `.zip`. Node's
  built-ins read none of these archive formats and have no xz decoder, so
  the module runs a system `tar`. macOS's is bsdtar, which reads zip. On
  Windows the module runs `%SystemRoot%\System32\tar.exe`, also bsdtar,
  by its full path and never as a bare `tar`: Windows runners also carry
  Git for Windows' GNU tar, which can come first on `PATH` and does not
  read zip. Linux's GNU tar does not read zip either, and today's shell
  uses `unzip` there; decide between that and a small zip reader on
  `zlib`'s raw inflate. Test each OS's choice, and unpack a zip on the
  Windows leg.
- **The wrapper runs in three steps.** `install-omnisim` (179 lines, used
  by six workflows) becomes a composite wrapper: the JavaScript action
  first resolves the asset and its pin and outputs the cache key, which
  carries the pin so that a pin bump misses the old cache; then
  `actions/cache` restores with that key; then the JavaScript action runs
  again, downloading, verifying and unpacking on a miss and exporting the
  paths either way. `install-astap` and the ZWO SDK's Linux and macOS
  path have the same shape.
- **Done when** `install-omnisim`'s bash is gone, every workflow that uses
  it is green on each venue a branch can reach, and the module's tests
  cover asset choice (arm64 Linux included), a checksum mismatch and a
  truncated download. The Pi's first nightly and bazel.yml's first
  push-to-main macOS leg after merge are watched, since no branch run
  reaches them.

### J3–J6 — The other install actions

Each moves onto J2's module in its own PR. What each brings that J2 did
not:

- **J3 `install-pebble`** (130 lines). `scripts/install-pebble.sh` mirrors
  its pin table for developers; after this item there is one table both
  read, or the script goes.
- **J4 `install-astap`** (283 lines). The D05 star database as a second
  asset, and SourceForge mirrors: today's curl flags re-roll a stalled
  mirror (the step's comments explain the numbers), which the module's
  stall timeout must reproduce. `refresh-astap-shas.yml` both reads the
  current pins from this `action.yml` (awk, failing when the table changes
  shape) and rewrites them there when upstream rotates. J4 points both
  steps at the new table in the same PR, or a rotation after J4 opens a
  refresh PR that leaves the real pins unchanged; J8 then replaces the
  rest of that workflow. That workflow checks out `main` in every job, so
  the two steps are proven by tests against a copy of the table, not by a
  branch dispatch.
- **J5 `install-zwo-sdk`** (442 lines, bash and PowerShell). A sudo and a
  sudo-free Linux path, a macOS path and a Windows path; the cache restore
  and save are separate steps; the Windows download is the unpinned,
  weekly-cached one. `check-pkg-assets.sh` reads this action's `ref`
  default and Windows URLs, so they stay where it reads them or it
  changes in the same PR.
- **J6 `install-svbony-sdk`** (367 lines, bash and PowerShell). Windows
  downloads from the project's private mirror, against a SHA-256; Linux
  and macOS from the INDI repository, against a git ref.
  `check-pkg-assets.sh` reads the `ref` default here too.

### J7 — Nightly tracking-issue action

Ten workflows each carry a `github-script` block that opens a tracking
issue when a scheduled run fails (safety.yml and scheduled.yml also when
it is cancelled), or comments on the open one. One action with `title`,
`labels` and `body` inputs replaces them. Each block lists the open
issues carrying its own label, picks the one with its exact title, and
creates with that label and `ci-flake`; triage relies on both. The labels
follow no pattern, so the action derives nothing from a workflow's name,
and each workflow passes the label it uses today:

| Workflow | Label |
| -------- | ----- |
| check.yml | `check-nightly` |
| conformu.yml | `conformu-nightly` |
| install-astap.yml | `install-astap-nightly` |
| native.yml | `native-nightly` |
| nightly-packages.yml | `nightly-packages` |
| pi-nightly.yml | `pi-nightly` |
| plate-solver-smoke.yml | `plate-solver-nightly` |
| safety.yml | `safety-nightly` |
| scheduled.yml | `rolling-nightly` |
| ui-browser-nightly.yml | `browser-nightly` |

Each workflow keeps its exact title and label too, because others read
them. `refresh-astap-shas.yml` finds install-astap's tracking issue by
its title and its `install-astap-nightly` label to link a rotation PR to
it. `clippy-beta`'s per-lint census issues also carry `check-nightly`, so
only the exact title tells check.yml's tracking issue from them.

Those jobs check nothing out today, and declare only `issues: write`, so
`contents` is none. The action needs a sparse checkout of itself and of
wherever F1 put the helper, so each job also gets `contents: read`.
`refresh-astap-shas.yml`'s rotation issue and `clippy-beta`'s per-lint
census are different and stay out unless they fit without special
cases.

The action runs only after a scheduled run fails or is cancelled, and a
dispatched run is never a scheduled one. Its tests run the open-or-comment logic against a fake GitHub
API, and the PR proves it live with a temporary dispatchable workflow
that calls the action against a throwaway title and label, then closes
what it opened; the temporary workflow is deleted before merge. After
merge, the first scheduled failure is checked for its issue.

### J8 — ASTAP pin refresh

`refresh-astap-shas.yml` is the largest block of workflow shell (260
lines): it detects an upstream rotation per platform, checks a new binary
solves M101, rewrites the pins and opens or updates a PR. After J4 the
pins live in J4's table, which J4 has already pointed both the read and
the rewrite at. Every job checks out `main` and the run opens a real PR,
so a branch dispatch proves nothing: the proof is tests against fixture
tables and recorded upstream responses, then a watched run after merge.

### J9 — Release and nightly publishing glue

`nightly-packages.yml` (the version dialects, the skip when the nightly
already has this SHA, the rolling prerelease publish, the rpm asset names,
the tap push, the previous MSI fetch) and `release.yml` (the tag-version
check, the formula commit and push). Split it per workflow if the PR gets
large.

A dispatch of `nightly-packages.yml` on the branch with `dry_run=true`
proves the `plan` job (the version dialects, the skip when the nightly
already has this SHA) and the `msi` job's fetch of the previous MSI,
which all run in a dry run. Without `dry_run=true` the dispatch publishes
the branch as the public nightly, so never run it that way. The `publish`
job (the prerelease, the rpm asset names, the tap push) is skipped by the
dry run, and `release.yml` runs only on a `v*` tag. That code is tested
against a fake GitHub API and a temporary tap checkout. A first PR may
add a dry run that goes as far as computing what it would publish and
printing it. The PR names what stays unproven, and someone watches the
next nightly after merge. The release half's first live run is a real
release; plan it for one that someone is watching.

### J10 — Bazel remote-cache flags

`tools/ci/bazel-remote-flags.sh` is `source`d by bazel.yml and
bazel-coverage.yml to build a `REMOTE_FLAGS` array per venue. It is
sourced so the array, which can carry the cache write credential, stays
in the step's own shell and never reaches the runner's disk. Its header
gives the reason as the next job on a pool VM reading it, but every venue
that runs these workflows is single-use (the pool's clones are destroyed
after their job, ADR-020; hosted runners likewise), so the real exposure
of a file is to the later steps of the same push-to-main job. An action
that writes the venue's flags to a bazelrc file the workspace `.bazelrc`
imports with `try-import` would remove the sourcing from every Bazel step.
There is a second exposure: `.bazelrc` sets `build:ci --announce_rc`, so
every Bazel step prints the options it read from rc files, and a
`--remote_header=Authorization=…` in an imported file would be printed in
every push-to-main step, behind nothing but GitHub's secret masking. So
the credential cannot go in the file at all: keep it out (a credential
helper that reads it from the step's environment is the first to check),
and correct the script's header either way. If no such way works, drop
this item and keep the script. The mask step stays inline in the YAML either way:
it runs before checkout, and a local action cannot. The macOS venue's
flags run only in bazel.yml's macOS leg, which no branch reaches, so
tests prove them and the first push to main is watched.

### J11 — Bazel/Cargo target parity

`scripts/check-bazel-cargo-parity.sh` (82 lines), run by `parity.yml`
alone (on push to main, nightly and on dispatch), compares the targets
`cargo metadata` lists with the ones `bazel query` finds. It was R3 until
the plan's second review: only CI runs it, so ADR-022's tiebreak puts it
in JavaScript, and a Rust port would compile the tools crate cold in a
job that has no Rust cache. The action runs the two commands and compares
their output. It is tested against fixture outputs, and a dispatched
`parity.yml` run on the branch proves it (that workflow publishes
nothing); after merge, the push-to-main run is the first to watch.

### J12 — Sharded BDD runner

The `bdd-sharded` composite action (157 lines of bash) runs the BDD
suites as parallel shard processes for the Cargo safety net (`test.yml`
three times, `scheduled.yml` three times): it starts the processes, sets
the shard variables only on the suites that honour them, and collects
their exit codes and logs. Only CI runs it, so under ADR-022's tiebreak
it becomes a JavaScript action; it was R4 until the plan's third review.
Its tests cover which suites get shard variables and that one failing
shard fails the step. Neither workflow runs on a PR, and neither
publishes, so dispatched `test.yml` and `scheduled.yml` runs on the
branch prove it.

### J13 — Package-repository scripts

`build-apt-repo.sh`, `build-yum-repo.sh`, `verify-packages-repo.sh` and
`push-packages-repo.sh` (725 lines) build, check and upload the apt
and yum repositories. Only `nightly-packages.yml` runs them, and no doc
sends a developer to them, so under ADR-022's tiebreak they are
JavaScript; the other packaging scripts are R8's. One PR, or two (the
builders and their verifier, then the push). The first three run in the
`repo` job, which a `dry_run=true` dispatch runs, so that dispatch
proves them. `push-packages-repo.sh` runs only in `publish`, so it falls
under the publishing rule: tests against a fake store first, then a
watched nightly. `tools/rusty-photon-packages-r2/README.md` points to the
push script's header for the upload order, and changes with it.

### J14 — Crates from the LAN bazel-remote

The pool `stable / clippy` downloads every crate it compiles from
crates.io, over the operator's WAN. The LAN bazel-remote can serve them:
its HTTP CAS keeps a blob under its SHA-256, which for a crate is the
`checksum` that `Cargo.lock` records, and anyone on the runner network
can read it. The owner chose this over a new caching service on
2026-10-13; T1 does the same for the toolchain.

- **Today the CAS holds few of them.** Bazel stores a crate there only
  when it fetches the crate through the Remote Asset API, which only
  authorized runs do, and only for crates the template's repository cache
  lacks ([proxmox-runner-pool.md](../skills/proxmox-runner-pool.md)). On
  2026-10-13 it held 60 of the 583 crates.io packages in `Cargo.lock`, and
  it was full, evicting the least recently used. So this action fills it
  as well as reading it.
- **The action** (`runs.using: node24`) runs before any Cargo step. For
  every crates.io package in `Cargo.lock` it reads `/cas/<checksum>`,
  fetches the crate from crates.io on a miss, checks the SHA-256 against
  `Cargo.lock` either way, and puts the file where Cargo looks for
  downloaded crates. A cache that is down or slow is a miss, never a
  failed job.
- **Integrity:** every crate is checked against `Cargo.lock`, the repo's
  own pin, whoever served it. Unlike J1's archive, the restore is
  therefore safe in the push-to-main run too. (On a PR run, the
  `target/` that J1 restores is unchecked either way; J1 keeps
  `registry/src` out of its archive so the sources at least come from
  these checked files.)
- **Git dependencies are not crates.** The workspace's one git
  dependency, ascom-alpaca pinned to a commit on the org fork, has no
  crates.io checksum, so the action leaves it alone and Cargo clones it
  from GitHub. J1's archive carries `$CARGO_HOME/git` for PR runs; a
  push-to-main run still clones it.
- **Filling the CAS:** on push to main only, the action uploads each crate
  it had to fetch, with the LAN cache's write credential
  (`BAZEL_LAN_CACHE_WRITE_AUTH`, a full `Authorization` value), passed as
  an input with bazel.yml's push-only expression. The uploads finish
  before any Cargo step starts, so no build script runs while the job
  holds the credential. bazel-remote checks an uploaded CAS blob's
  SHA-256 against its key (`sha256verifier` in its `cache/disk/disk.go`,
  for uncompressed storage); confirm it does with the store's settings.
  Reading the crates on every run keeps them from being evicted.
- **Where Cargo looks:** `$CARGO_HOME/registry/cache/<index dir>/` holds
  `<name>-<version>.crate`, under a directory name Cargo derives (today
  `index.crates.io-1949cf8c6b5b557f`) that has changed between Cargo
  releases. Either the action finds that name rather than hard-coding it,
  or it unpacks the crates into a `directory` source that replaces
  crates.io, which also spares Cargo the index requests. Decide in the
  item.
- **Endpoint:** `RP_LAN_CACHE_URL`, which the runner provides and the job
  already masks; no endpoint (hosted runners, fork PRs) means the action
  does nothing.
- **What the PR cannot prove:** the upload runs only on push to main.
  Tests against a fake CAS cover it before merge; after merge, watch the
  first push to main fill the CAS and the next PR read every crate from
  it.
- **Done when** a pool `stable / clippy` run on a PR whose lockfile main
  has already built downloads no crate from crates.io, and the job still
  passes with the cache stopped. The git dependency is outside this
  check; its clone is J1's to cache.

## Rust items

### R1 — Toolchain checksums

`scripts/rust-toolchain-sha256s.sh` (124 lines), which pre-push.md sends
developers to when a Rust pin moves: it prints MODULE.bazel's `sha256s`
entries from a release's channel manifest, after checking the manifest's
detached signature against the Rust release key, pinned by fingerprint.
Today `gpg` does the check. The port either keeps calling `gpg` or takes
an OpenPGP crate; decide in the item. The manifest parsing is unit-tested
against a saved manifest.

`tools/ci/bazel-clippy.sh` keeps its own copy of the archive list this
script prints, and its errors tell developers to run it. R1 updates those
messages in the same PR, and exposes the list so R2 uses it instead of
keeping a second copy.

### R2 — Bazel clippy

`tools/ci/bazel-clippy.sh` (184 lines): runs the clippy aspect over
`//...` and checks the toolchain pins agree. It is in AGENTS.md rule 4's
local gate and in bazel.yml's `bazel clippy` step. It needs R1 first, and
reads R1's archive list rather than porting the script's own copy.
**Open question:** the tool may not start through the alias in the Bazel
legs. The Linux pool template has no Cargo until T1, and whether the
Windows template, whose variables include `CARGO_HOME` and `RUSTUP_HOME`
([proxmox-runner-pool.md](../skills/proxmox-runner-pool.md)), puts Cargo
on a job's PATH is unchecked. `bazel run` would make the
tool start Bazel from inside a `bazel run`; building it first and running
it from `bazel-bin` avoids that. Verify the choice on the Linux and
Windows PR legs; don't assume it. bazel.yml's macOS leg runs the step
too, but never on a branch, so the launch there is proven by the first
push to main (see the proving rule).

### R3 — Moved: Bazel/Cargo target parity

Now J11.

### R4 — Moved: sharded BDD runner

Now J12.

### R5 — Moved: workspace discovery and the MSRV check

check.yml's "Verify workspace MSRV" (a `while read` loop),
scheduled.yml's "Find Miri services" and conformu.yml's "Discover
ConformU services" (each an `if`/`else` on a count, writing several
outputs) read `cargo metadata | jq`. They have control flow, so ADR-022
does not leave them in YAML, and only CI runs them, so they are
JavaScript under its tiebreak, which also spares those jobs a compile of
the tools crate. They are in S1's list.

### R6 — Pool routing copies stay identical

The pool's `runs-on` routing expression is copied into bazel.yml,
bazel-coverage.yml, repin-bazel.yml and check.yml. ADR-020 makes it a
security boundary, and only a comment keeps the copies in step. They
come in two shapes: bazel.yml holds the Linux and Windows clauses in one
expression, each prefixed `matrix.os == '…' &&`, with `|| matrix.os` as
the fallback, while the others are standalone and end in
`|| 'ubuntu-latest'`. The test in the tools crate extracts each OS's
clause from every copy (the kill-switch variable, the trusted-event test
and the pool labels, without the matrix prefix or the fallback) and
asserts each equals the reviewed clause for that OS, held in the test
itself. A PR that changes one copy fails `bazel test //...`, and a PR
that changes them all has to change the test too, in the same diff the
reviewer reads. The test reads files under `.github`, so it needs F1's
answer on `.bazelignore`. Raised by the review of #1505.

### R7 — ConformU runner

`scripts/test-conformance.sh` (450 lines), which hardware-validation.md
and pre-push.md send developers to. conformu.yml's "Run ConformU tests
(parallel)" block may fold into the same subcommand.

### R8 — Packaging scripts

The packaging scripts a developer also runs, so Rust under ADR-022's
tiebreak: about 3,750 lines in 8 files. The packaging docs send
developers to `build-packages.sh` and `verify-packages.sh`
(packaging.md), `build-tarballs.sh` and `verify-brew.sh`
(packaging-macos.md), `build-msi.ps1` and `verify-msi.ps1`
(packaging-windows.md, installer/README.md) and `check-pkg-assets.sh`
(packaging/README.md); `verify-brew.sh` runs
`generate-brew-formulas.sh`. The apt and yum repository scripts, which
only CI runs, are J13.

One PR per script, or per tight pair (a builder and its verifier). A
builder or verifier is proven before its shell is deleted, by a
`nightly-packages.yml` dispatch with `dry_run=true` (which builds and
verifies every leg and publishes nothing) or by `msi.yml`. A script that
only the publish job or `release.yml` runs falls under the publishing
rule in "Working an item": tests first, then a watched first run on main.
The `verify-msi.ps1` PR also takes nightly-packages.yml's "Verify the MSI
lifecycle" step, whose only logic is whether to pass `-UpgradeFrom`.
ADRs 012 to 018 and the packaging docs name these scripts and change
with them.

## S1 — Sweep of the remaining inline blocks

Every workflow block with logic (control flow, or more than five lines)
that no item above takes and that "Not in this plan" does not leave out,
as of 2026-10-13. Only CI runs these, so each moves to JavaScript under
ADR-022's tiebreak, or stays in YAML when it is really one command, or
when a matrix value or an expression can replace its control flow. Small
PRs, in any order:

- bazel.yml: the Windows profile flags chosen in `bazel build`,
  `bazel clippy`, `bazel test (fast)` and `bazel test (BDD)`, once J10
  has taken their sourcing;
- bazel-coverage.yml: "bazel coverage" (its timeout and exit-status
  handling), "Annotate uncovered diff lines", "Capture bazel test logs",
  "Split combined lcov per package";
- check.yml: "Verify workspace MSRV" (from R5); shellcheck's "Install
  pinned shellcheck" and its script list; `clippy-beta`'s report and
  aggregation blocks, and its per-lint census `github-script` if J7
  leaves it out;
- conformu.yml: "Discover ConformU services" (from R5); the two "Export
  CONFORMU_PATH" blocks; "Run ConformU tests (parallel)", unless R7
  takes it;
- install-astap.yml: "Smoke test (Linux/macOS)" and "Smoke test
  (Windows)", a bash and PowerShell pair;
- parity.yml: the two "Guard - rules_rust …" steps, which J11 leaves;
- pi-nightly.yml: "Symlink libusb for the QHYCCD static link";
- plate-solver-smoke.yml: "Assert solve-time budget", "Log per-scenario
  timing";
- release.yml: "Strip binary";
- repin-bazel.yml: "Skip if already in sync", "Commit and push repin";
- safety.yml: "Enable opt-level 1 for sanitizers", and "cargo test"'s
  flags per sanitizer;
- scheduled.yml: "Find Miri services" (from R5).

The "Mask runner-provided cache endpoint" steps stay in YAML for the
reason J10 gives.

## T1 — The Linux pool template carries rustup and stable

The pool `stable / clippy` installs rustup and the stable toolchain over
the WAN on every run, because the Linux template has neither. The owner
chose on 2026-10-13 to put them in the template rather than run a caching
proxy; J14 serves the crates.

- **What the template carries:** rustup for the `ci` user, with the
  stable toolchain and the components the job's `dtolnay/rust-toolchain`
  step asks for (`clippy`, `rustfmt`), and `~/.cargo/bin` on the PATH the
  runner gives its jobs. The step installs rustup only when it finds none,
  and `rustup toolchain install stable` then finds nothing new to
  download while the template's stable is current.
- **When stable moves**, every six weeks, each run downloads the new
  release until the next rebuild. Each template rebuild therefore runs
  `rustup update stable`, and a rebuild after a Rust release keeps the
  step from downloading.
- **The doc:** proxmox-runner-pool.md's components list and its rebuild
  steps say what the template carries and how to check it before capture,
  as they do for the `diff-cover` pin: as `ci`, the step's own install
  command reports the toolchain unchanged.
- **The work is the operator's:** the rebuild happens outside the repo,
  and the item's PR is the doc change, merged once the template is rolled.
  Hosted runners are unaffected; they carry their own rustup, so the
  Linux Bazel legs already run with Cargo on the PATH there, and the
  pool's will too.
- **Every mention moves with it.** These say the Linux pool has no Cargo
  or rustup, or that the job downloads its toolchain, and change in the
  same PR: ADR-022's Consequences, F2's note on `repin-bazel.yml`, R2's
  open question, J1's scope bullet, and the comment above check.yml's
  `stable / clippy` job.
- **Done when** a pool `stable / clippy` run's toolchain step installs no
  rustup and downloads no toolchain.

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

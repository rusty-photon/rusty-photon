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
| F1  | JavaScript foundation: helper module, Node test rule, conventions | — | — | Done (#1508) |
| F2  | Rust tools crate, with its first tool (`repin-bazel-lock`) | `scripts/repin-bazel-lock.sh` | — | Done (#1541) |
| J1  | Cargo build cache in the LAN bazel-remote | nothing (adds a cache to the pool `stable / clippy`; rust-cache stays for its hosted runs) | F1 | Done (#1534) |
| J2  | Shared download-verify-extract module, first used by `install-omnisim` | `install-omnisim` | F1 | Not started |
| J3  | `install-pebble` on the module | `install-pebble` | J2 | Not started |
| J4  | `install-astap` on the module | `install-astap` | J2 | Not started |
| J5  | `install-zwo-sdk` on the module | `install-zwo-sdk` (bash + PowerShell) | J2 | Not started |
| J6  | `install-svbony-sdk` on the module | `install-svbony-sdk` (bash + PowerShell) | J2 | Not started |
| J7  | Nightly tracking-issue action | 10 `github-script` copies | F1 | Done (#1527) |
| J8  | ASTAP pin refresh | `refresh-astap-shas.yml`'s bash | F1, J4 | Not started |
| J9  | Release and nightly publishing glue | `nightly-packages.yml` / `release.yml` logic blocks | F1 | Not started |
| J10 | Bazel remote-cache flags | `tools/ci/bazel-remote-flags.sh` | F1 | Not started |
| J11 | Bazel/Cargo target parity | `scripts/check-bazel-cargo-parity.sh` | F1 | Not started |
| J12 | Sharded BDD runner | `bdd-sharded`'s bash | F1 | Not started |
| J13 | Package-repository scripts | `build-apt-repo.sh`, `build-yum-repo.sh`, `verify-packages-repo.sh`, `push-packages-repo.sh` | F1 | Not started |
| J14 | Crates from the LAN bazel-remote | nothing (the pool `stable / clippy` stops downloading crates from crates.io) | F1 | Done (#1519) |
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

Done in #1508. [ci-logic.md](../skills/ci-logic.md) has the conventions it
set; what later items build on:

- **Node for tests.** rules_nodejs in `MODULE.bazel` pins a Node 24, the
  major version of the actions' `runs.using: node24`.
- **Where everything lives.** `.github` is out of `.bazelignore` (it had
  been there since #82, with no reason recorded; nothing under it had a
  BUILD file). The helper and every action's tests live in one Bazel
  package, `.github/actions`, with no BUILD file below it.
- **`node_test`** (`.github/actions/node_test.bzl`) runs `node --test` as
  an ordinary Bazel test on Linux, macOS and Windows; the Windows
  launcher is a batch file.
- **`link_test`** parses and links every JavaScript file in the package
  without running it, so an entry file that no test imports still fails
  the PR on a syntax error, a wrong import, an npm package or a `.js`
  file. It replaced the planned `node --check` in review, since parsing
  alone missed imports.
- **`lib/helpers.mjs`**: inputs, the `GITHUB_OUTPUT`/`GITHUB_ENV`/
  `GITHUB_PATH` files, masking, log groups, failing the step, `fetch` with
  timeouts and bounded retries, SHA-256 of a stream.
- **Proven** by two deliberately red commits, each failing on both PR legs
  (a broken helper; an unimported entry file that does not parse, then
  one importing a name that does not exist), and reverted.

### F2 — Rust tools crate

Done in #1541, as `tools/rusty-photon-ci`, run as `cargo ci <tool>`.

- **The crate:** `tools/rusty-photon-ci`, a workspace member with
  `publish = false`, under the workspace lints and MSRV, built and tested
  by Bazel and Cargo (it joins the parity check like any crate). It sits
  under `tools/`, not `crates/`, because no service links it. A library
  holds the tools and their tests; the binary only parses the command
  line, as the services' binaries do. [ci-logic.md](../skills/ci-logic.md#the-rust-tools-crate)
  has the conventions for adding a tool.
- **`cargo ci <tool>`:** a Cargo alias in `.cargo/config.toml`
  (`run --package rusty-photon-ci --`), so developers have a short command
  that needs nothing but Cargo. Its first run compiles the crate.
- **A tool takes a `Runner`.** It builds each command as data (program,
  arguments, directory, the variables it sets and removes), so its tests
  check what it would run, in what order and with what environment,
  against a fake, with no Bazel or git on the test's host.
- **First tool:** `repin-bazel-lock`, replacing
  `scripts/repin-bazel-lock.sh` (60 lines): it refreshes and stages
  `MODULE.bazel.lock`, and AGENTS.md rule 10 and the pre-commit hook send
  developers to it. It touches no CI step, so it proves the crate without
  risking one. `repin-bazel.yml`'s "Repin crate index" step runs the same
  sequence on dependabot PRs, on the Linux pool, which has no Cargo until
  T1. That step is a fixed sequence of three commands with no control
  flow, which ADR-022 leaves in YAML; its "keep the two in step" comment
  points at the tool.
- **Running Bazel under `cargo run`** gives it Cargo's variables
  (`CARGO_*`, the library path, `.cargo/config.toml`'s `[env]`). None
  reaches the lock: it records only crate_universe's own variables, which
  the tool removes, and `--incompatible_strict_action_env` keeps the rest
  out of the actions.
- **Proven locally:** on the same tree, the tool and the script wrote a
  byte-identical lock, and printed the same report of unstaged Cargo
  files. Run with `REPIN`, `CARGO_BAZEL_ISOLATED` and `CARGO_BAZEL_DEBUG`
  exported, the tool wrote that lock again; `bazel mod tidy` run directly
  with `CARGO_BAZEL_DEBUG=1` wrote a lock recording it.
- **What named the script:** about twenty files, the pre-commit hook's
  messages among them (`tools/ci/check_bazel_lock.py`'s refusals, which
  are what a developer sees). All of them name the tool now, except this
  plan's record of what F2 replaced, and the archived plan and ADR-022's
  context, which now say "the lock repin".
- **Done when** the shell script is gone, nothing in the repo names it,
  and the tool's report of unstaged Cargo files is unit-tested.

## JavaScript items

### J1 — Cargo build cache in the LAN bazel-remote

Done in #1534, as `.github/actions/cargo-cache`. #1505 moved the
`stable / clippy` job to the pool, where Swatinem/rust-cache cannot help:
its cache lives in GitHub, across the
operator's WAN, so the job runs it on hosted runners only. This item
gives the pool runs a cache on the LAN; the hosted runs keep rust-cache.

- **Scope: the compile.** On the pool the job's cold clippy passes take
  about 110 s of its 2m13s. Installing the toolchain takes about 10 s more,
  over the WAN, because the pool template has no rustup; T1 puts it in the
  template. Downloading the crates is J14's.
- **Store: the LAN bazel-remote**, by the owner's decision of 2026-10-15,
  over the S3-compatible object store chosen on 2026-10-13, which the
  first spike below used. It needs no new service, secret or signing
  code. Its HTTP CAS keeps each archive under its SHA-256, so a restore is
  checked for free, and it refuses an upload whose hash is not its key
  (J14). Its eviction of the least recently used entries retires the
  archives of old toolchains and templates, which no run reads. The
  transport stays plain HTTP: TLS on the LAN cache was weighed the same
  day and left for later.
- **The pointer** from a key to the newest archives is an action-cache
  entry: an `ActionResult` that names each archive as an output file.
  bazel-remote v2.6.2, the NAS's version, takes and gives one as JSON
  (`Content-Type` and `Accept: application/json`), so the action encodes
  no protobuf. It refuses an entry whose digest is malformed, and answers
  404 for one that names an archive it no longer holds, or holds at
  another size, so an evicted archive is a cold build. It records the
  writer's address in an entry's `worker` unless the entry names one, and
  anyone can read the entry back, so the action names its own. Checked
  against a local v2.6.2 on 2026-10-15, and in its source (`server/http.go`:
  `addWorkerMetadataHTTP`, `handleGetValidAC`).
- **The action:** `.github/actions/cargo-cache/`, `runs.using: node24`.
  Its main step restores; its `post:` step saves, with `post-if:
  success()`: a post step otherwise runs after a failed or cancelled job
  too, and would replace the one good cache with a half-built one. No
  endpoint means it does nothing, which covers hosted runners and fork
  PRs. It runs after the toolchain install, which the key reads, and
  before `cargo fmt`.
- **Endpoint:** `RP_LAN_CACHE_URL`, the bazel-remote lan-crates reads, so
  no runner `.env` entry and no template rebuild. Masked before use, with
  its host and hostname; what the action prints about a failed request is
  its status or error codes, never fetch's message, which names the
  address it connected to. Both helpers moved to `lib/helpers.mjs`
  (`maskUrl`, `requestFailure`), as lan-crates uses them too.
- **Write key:** `BAZEL_LAN_CACHE_WRITE_AUTH`, passed only on push to main
  with the expression bazel.yml uses
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
  main replaces the archive with a cold build. bazel-remote has no
  pre-signed URLs, so the alternative weighed here before, a job that
  builds nothing holding the key and handing the build job a URL for one
  object, is not open. Like Bazel's and lan-crates', the credential
  crosses the runner VLAN in clear text.
- **Writers never restore.** A push-to-main run builds cold and saves; only
  PR runs restore. The archive holds build-script and proc-macro binaries
  that the job runs, and the pointer that names it is read over plain
  HTTP, so a tampered restore must never run in a job that holds the
  write key, or be saved back for every later PR. A cold build on main
  also means the archive holds only what the current lockfile needs.
- **What is cached:** two archives, each unpacked only under its own root.
  `target/`, without the workspace's own units: their fingerprint and
  build-script directories (named after the package), their compiled
  files (named after the target), the incremental state and the examples.
  Every run lints those crates anyway, and a restored fingerprint could
  let Cargo skip linting one. And `$CARGO_HOME/git`, for the workspace's
  git dependency. Not the crates.io crates: J14 serves them from the LAN
  as a directory source, unpacked and checked against `Cargo.lock` on
  every run, so Cargo writes nothing under `$CARGO_HOME/registry` on the
  pool, and a restored copy of the sources Cargo compiles would bypass
  that check. `target/` records those sources by absolute path (see the
  first spike below), so J14's directory must keep one path from run to
  run, as `$RUNNER_TEMP/lan-crates` does on the pool. Packed by GNU tar
  with `--zstd`, on as many threads as the runner has (`ZSTD_NBTHREADS`,
  from zstd's manual), or uncompressed on a runner without zstd; the
  pointer names which.
- **The crates' mtimes, a fix to J14.** The first measurement with
  bazel-remote saved almost nothing: 84 s warm against 94 s cold. Cargo's
  own unpacking into its registry keeps each file's mtime from the tar
  header, and `cargo package` writes a fixed one (2006-07-23); lan-crates
  wrote each file at the time of the run. A build script that declares
  `rerun-if-changed` reruns when a file it names is newer than its last
  run, so after a restore every such build script reran, and every crate
  that depends on its crate was rebuilt. lan-crates now keeps the header's
  mtime, and gives the directories and `.cargo-checksum.json` the crate's
  newest.
- **Key:** the SHA-256 of the job's ID, `rustc -vV`, a hash of the
  runner's system packages (`dpkg-query`), and the paths Cargo records:
  the workspace, `CARGO_HOME` and `RUNNER_TEMP`, under which lan-crates
  unpacks. The packages stand in for the pool template: `target/` holds
  build-script outputs (pkg-config probes, bindgen output) that Cargo does
  not rebuild when the template's system libraries change, and a template
  rebuilt with other packages gets a new key, with no version to keep in
  step by hand. Each push to main overwrites its key's pointer with its
  own cold build, so the archives never gather the dependencies of older
  lockfiles; the keys of older toolchains and templates, which no run
  reads, are evicted with their archives.
- **Order between writers:** two pushes to main close together run
  concurrently, and the older commit can finish last. The save first
  reads the head of the default branch (the GitHub API, with the job's
  token) and skips if it is not the run's commit, so the newer push's own
  save is left to that push. The save writes the archives first and the
  pointer last, so the pointer never names an archive the store lacks.
- **Integrity:** each archive is checked against the size and SHA-256 its
  pointer names before anything is unpacked, and a restore that fails part
  way takes back what it unpacked. The pointer itself is read over plain
  HTTP and checked against nothing: something on the runner VLAN that
  could answer for the cache could name archives of its own. That is the
  gap TLS would close. Bazel's own reads of the same cache, whose action
  results name the outputs a build then runs, have the same one.
- **Failure:** a missing pointer or archive, a slow store or a store that
  is down gives a cold run, never a failed job. Every request has a
  timeout: 10 s for the pointer, 300 s for an archive. A failed restore or
  save is a warning annotation on the run, so a broken path is visible and
  not just a cache that silently stays cold; a missing pointer is a line
  in the log, since every new toolchain starts with one.
- **First spike (2026-10-13, local Docker Garage, both clippy passes at
  `-j 6`, the same paths for both runs):** cold 113 s; after a restore,
  34 s. The restore took 1.2 s and the upload 0.6 s, for a 326 MiB
  archive at `zstd -3`. Restoring to different paths saved nothing,
  because Cargo fingerprints absolute paths; the pool's paths are fixed.
- **Measured with bazel-remote (2026-10-15, a local v2.6.2, both passes at
  `-j 6`, fixed paths, the crates from lan-crates):** cold, 96 s and 21 s.
  `target/` came to 4.5 GB, 837 MB without the workspace's own units, and
  234 MiB at `zstd -3`; packing, the upload and the restore took under a
  second each. With the crates' mtimes kept, the warm passes took 30 s and
  11 s, and pass 1 rechecked the workspace's 54 crates and nothing else. A
  planted unused variable in a workspace crate still failed pass 1 after a
  restore. The action itself, its main and post entry files run as the
  runner runs them: the writer saved 233 MiB and 2 MiB in 1.4 s after its
  cold passes (95 s and 21 s); the reader restored them in 1.0 s and ran
  warm in 34 s and 11 s.
- **What the PR cannot prove:** the save runs only on a push to main,
  since only that event gets the key, and a PR's own run finds no pointer
  until one has. The tests run the action against a fake bazel-remote
  that answers as v2.6.2 does, and break each guarded behaviour on
  purpose.
- **Proven after merge**, on 2026-10-15:
  - the merge's own push to main
    ([job](https://github.com/rusty-photon/rusty-photon/actions/runs/38849381705/job/116806978129))
    built cold and logged "Saved the build cache in 3.4 s: target 243 MiB,
    git 2 MiB.";
  - #1511, a dependency bump
    ([job](https://github.com/rusty-photon/rusty-photon/actions/runs/38850843667/job/116811503083)),
    logged "Restored the build cache in 2.1 s: 245 MiB", and its clippy
    passes took 44 s and 27 s, against 1m23s and 42 s cold on #1534's own
    run
    ([job](https://github.com/rusty-photon/rusty-photon/actions/runs/38849131769/job/116806224373)).
    Its bump changed the lockfile, so the crates it touched were checked
    again too;
  - #1535, a change to `rp`
    ([job](https://github.com/rusty-photon/rusty-photon/actions/runs/38851459832/job/116813359122)),
    restored the same archive in 2.1 s and passed in 33 s and 17 s;
  - when both merged, on 2026-10-16, their two pushes to main ran
    together. The older one finished first and logged that main had moved
    on, leaving the cache to the newer push
    ([job](https://github.com/rusty-photon/rusty-photon/actions/runs/38981023774/job/117241666183)),
    which saved it in 2.9 s
    ([job](https://github.com/rusty-photon/rusty-photon/actions/runs/38981086513/job/117241862959)).
- **Done when** a pool `stable / clippy` run on a PR restores the cache
  and runs warm, and a push to main builds cold and saves it: the runs
  above. A stopped store is left to the tests, as J14's was by the
  owner's decision of 2026-10-15: a refused connection, a store that does
  not answer in time,
  a refused upload and a wrong archive are each a cold build or an unsaved
  cache there, never a failed step.

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

Done in #1527, as `.github/actions/tracking-issue`. Ten workflows each
carried a `github-script` block that opened a tracking issue when a
scheduled run failed (safety.yml and scheduled.yml also when it was
cancelled), or commented on the open one. Their jobs now call the action,
with their `if:`, `needs:` and runners unchanged.

- **Inputs:** `title`, `label` and `body`. One label, not a list: each
  workflow searches by its own label, and the action adds `ci-flake` to an
  issue it opens, as every block did; triage relies on both. The labels
  follow no pattern, so the action derives nothing from a workflow's name,
  and each workflow passes the label it used before (below). `body` is the
  workflow's own text, what failed and what to suspect; the action adds
  the run's link and the note on how the issue works, which all ten
  carried. The issues it opens are the old ones byte for byte, except
  ui-browser-nightly's, whose note had been wrapped differently and now
  wraps like the other nine.
- **What it does:** lists the open issues carrying the label, every page,
  leaving out pull requests (the issues endpoint lists them too), and
  comments on the one with the exact title, or opens one. Neither POST is
  repeated on failure, since a repeat that GitHub had acted on would open
  a second issue. A next page on another origin than the API's is
  refused, since the token goes with each request. A new issue that
  comes back without the label fails the step: GitHub silently drops a
  label that does not exist, and the next failure, not finding the issue,
  would open another every time. The old blocks had that gap; the live
  proof's first run fell into it. A failure names what failed and carries
  GitHub's answer.
- **Titles and labels are unchanged,** because others read them.
  `refresh-astap-shas.yml` finds install-astap's tracking issue by its
  title and its `install-astap-nightly` label to link a rotation PR to it.
  `clippy-beta`'s per-lint census issues also carry `check-nightly`, so
  only the exact title tells check.yml's tracking issue from them.

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

- **Checkout:** an action under `.github/actions` is read from the
  checkout, and those jobs checked nothing out. Each now checks out the
  action's directory and `.github/actions/lib` only (`sparse-checkout`,
  `persist-credentials: false`), with `contents: read` beside
  `issues: write`.
- **Left out:** `refresh-astap-shas.yml`'s blocker issue, whose body has
  no such note and whose comment reads differently, and `clippy-beta`'s
  per-lint census, which opens and closes one issue per lint. Both would
  need special cases. The blocker goes with the rest of that workflow in
  J8; S1 sweeps the census.
- **Proof:** tests against a fake GitHub API, each shown to fail with its
  behaviour broken. Live on a hosted runner, from a temporary workflow
  that ran on a push to the PR's branch, since a workflow only on a
  branch cannot be dispatched ("To trigger the `workflow_dispatch` event,
  your workflow must be in the default branch", GitHub's docs): it opened
  a throwaway issue with both labels, commented on it, and closed it. The
  first two runs found the dropped label above, and that GitHub's issue
  list showed a just-opened issue only after about 4 s, a lag that only
  back-to-back calls meet.
- **After merge:** the jobs run only after a scheduled run fails or is
  cancelled, which no PR or dispatch produces; the first such failure is
  checked for its issue or comment.

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

Done in #1519, as `.github/actions/lan-crates`. The pool `stable / clippy`
reads every crates.io package `Cargo.lock` pins from the LAN bazel-remote's
HTTP CAS, which keeps a blob under its SHA-256: for a crate, the `checksum`
`Cargo.lock` records. The owner chose this over a new caching service on
2026-10-13; T1 does the same for the toolchain.

- **What it does**, as a step before any Cargo step: for every crates.io
  package in `Cargo.lock`, it reads `/cas/<checksum>` anonymously, fetches
  the crate from crates.io on a miss (`<dl>/<name>/<version>/download`,
  the URL Cargo uses), checks the SHA-256 against `Cargo.lock` either way,
  and unpacks it. Bytes from the cache that do not match are a miss, with
  a warning.
- **Where Cargo looks: a directory source.** Each crate is unpacked into
  `<name>-<version>/` with a `.cargo-checksum.json` whose `package` is the
  `Cargo.lock` checksum, which Cargo compares with the lockfile, and an
  empty `files` map, since the whole crate was checked before it was
  unpacked. `$CARGO_HOME/config.toml` makes that directory replace
  crates.io. Chosen over filling `registry/cache/<index dir>/` because
  Cargo then reads no index (no WAN request at all) and nothing depends on
  the directory name Cargo derives. The cost is unpacking every locked
  crate, other platforms' too, as Cargo's resolve needs every package in
  the lockfile: 583 crates, 64 MiB packed and 440 MiB on disk, in 1.5 s
  from a local bazel-remote. The tar reader is the action's own, on Node
  built-ins.
- **All or nothing.** If any crate cannot be had (crates.io failing, a
  crate whose bytes do not match `Cargo.lock`, one that does not unpack),
  the action writes no Cargo configuration and Cargo downloads its crates
  itself, with a warning naming the crate. A `CARGO_HOME` that already
  configures a source is left alone, with a warning.
- **A cache that fails** (any answer but a 404, a refused connection, no
  answer within 10 s) is asked no more in that run; the rest come from
  crates.io, with a warning. The first failed upload likewise stops the
  uploads, each tried once, so a cache whose writes hang costs one
  timeout, not one per crate. Never a failed step, and neither is a
  `Cargo.lock` the action cannot read: that is a warning, and Cargo
  downloads as before.
- **Filling the CAS:** on push to main only, each crate fetched from
  crates.io is uploaded with `BAZEL_LAN_CACHE_WRITE_AUTH`, passed to the
  step as an input under bazel.yml's push-only expression. The uploads
  finish inside the step, before any Cargo step. bazel-remote v2.6.2, the
  NAS's version, refuses a CAS upload whose SHA-256 is not its key with
  zstd storage, which is its default and what the NAS's `/status` shows
  (stored size well under the uncompressed size): a 500, "checksums don't
  match", and nothing stored, checked against a local v2.6.2 on
  2026-10-14. Reading the crates on every run keeps them from being
  evicted.
- **Git dependencies are not crates.** The workspace's one git dependency,
  ascom-alpaca pinned to a commit on the org fork, has no crates.io
  checksum, so Cargo clones it from GitHub. Caching it is J1's.
- **Endpoint:** `RP_LAN_CACHE_URL`, which the runner provides; without one
  (hosted runners, fork PRs) the action does nothing. What it prints
  about a failed request is the HTTP status or the error codes, never
  fetch's message: that names the address the host resolved to, which no
  mask of the URL covers. It masks the URL, its host and its hostname as
  well.
- **On 2026-10-14 the CAS held 60 of the 583 crates.io packages in
  `Cargo.lock`**, those Bazel had fetched through the Remote Asset API,
  and it was full (about 247 GB), evicting the least recently used. The
  crates add at most 64 MiB to it, evicting as much of Bazel's oldest
  entries.
- **What the PR could not prove:** the upload runs only on push to main.
  Tests against a fake CAS and a fake crates.io cover it. Against a local
  bazel-remote v2.6.2, the action took 582 crates from crates.io and
  uploaded them in 2.4 s, then served all 583 from the CAS in 1.5 s; Cargo's
  `fmt --check` and both clippy passes then ran on that directory with a
  fresh `CARGO_HOME`, downloading nothing.
- **Proven after merge**, on 2026-10-14:
  - the merge's own push to main
    ([job](https://github.com/rusty-photon/rusty-photon/actions/runs/38677070592/job/116232983685))
    read 61 crates from the LAN cache, fetched 522 from crates.io and
    uploaded all 522;
  - the next push, a dependency bump
    ([job](https://github.com/rusty-photon/rusty-photon/actions/runs/38677152335/job/116233245405)),
    read 581 from the cache and fetched only the 2 the bump added,
    uploading both;
  - a PR on that lockfile, #1527
    ([job](https://github.com/rusty-photon/rusty-photon/actions/runs/38679877014/job/116241751234)),
    logged "583 crates from Cargo.lock in 3.3 s: 583 from the LAN cache, 0
    from crates.io." and passed.
- **Done when** a pool `stable / clippy` run on a PR whose lockfile main
  has already built downloads no crate from crates.io ("583 from the LAN
  cache, 0 from crates.io"): #1527's run above. The git dependency is
  outside this check; its clone is J1's to cache.
- **A stopped cache is left to the tests**, not a live run: a refused
  connection, a cache that does not answer in time and one that answers
  with an error are each a miss there, and the crates come from
  crates.io. The cache is the bazel-remote the pool's Bazel legs build
  from, and with the Remote Asset API on they fail outright while it is
  unreachable ([proxmox-runner-pool.md](../skills/proxmox-runner-pool.md)),
  so no PR goes green with it stopped anyway. The owner chose this on
  2026-10-15, in place of the plan's live check that the job passes with
  the cache stopped.

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
reviewer reads. The test reads files under `.github`, which F1 took out
of `.bazelignore`. Raised by the review of #1505.

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

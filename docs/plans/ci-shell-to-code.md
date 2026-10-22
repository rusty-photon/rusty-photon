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
    remote flags) is proven by tests. Every item that changes
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
  - `refresh-astap-shas.yml` reads install-astap's SHA table, which J4
    moved into `install-astap/assets.json`, and rewrites it in its pull
    request, through the `refresh-astap` action since J8;
  - `scripts/check-pkg-assets.sh` reads the ZWO and SVBony `ref`
    defaults and the ZWO Windows URLs from their `action.yml`, to check
    the packages link what CI links;
  - the tools crate's test `toolchain_pins.rs` reads `rust.toolchain`'s
    `versions` and `sha256s` in `MODULE.bazel`, and the root Cargo.toml's
    `rust-version`, and holds the keys to `Release::archives` (R1, R2).

  An item that moves a pin updates its readers and writers in the same
  PR, or leaves the pin where they find it.

## Items

| ID  | Item | Replaces | Needs | Status |
| --- | ---- | -------- | ----- | ------ |
| F1  | JavaScript foundation: helper module, Node test rule, conventions | — | — | Done (#1508) |
| F2  | Rust tools crate, with its first tool (`repin-bazel-lock`) | `scripts/repin-bazel-lock.sh` | — | Done (#1541) |
| J1  | Cargo build cache in the LAN bazel-remote | nothing (adds a cache to the pool `stable / clippy`; rust-cache stays for its hosted runs) | F1 | Done (#1534) |
| J2  | Shared download-verify-extract module, first used by `install-omnisim` | `install-omnisim` | F1 | Done (#1564) |
| J3  | `install-pebble` on the module | `install-pebble` | J2 | Done (#1566) |
| J4  | `install-astap` on the module | `install-astap` | J2 | Done (#1573) |
| J5  | `install-zwo-sdk` on the module | `install-zwo-sdk` (bash + PowerShell) | J2 | Done (#1575) |
| J6  | `install-svbony-sdk` on the module | `install-svbony-sdk` (bash + PowerShell) | J2 | Done (#1578) |
| J7  | Nightly tracking-issue action | 10 `github-script` copies | F1 | Done (#1527) |
| J8  | ASTAP pin refresh | `refresh-astap-shas.yml`'s bash | F1, J4 | Done (#1580) |
| J9  | Release and nightly publishing glue | `nightly-packages.yml` / `release.yml` logic blocks | F1 | In progress: the nightly's plan and its MSI job's fetch (#1597), and its publish (#1598); release.yml next |
| J10 | Bazel remote-cache flags | `tools/ci/bazel-remote-flags.sh` | F1 | Done (#1585) |
| J11 | Bazel/Cargo target parity | `scripts/check-bazel-cargo-parity.sh` | F1 | Dropped (#1583): the check went, its workflow keeps the rules_rust guards |
| J12 | Sharded BDD runner | `bdd-sharded`'s bash | F1 | Done (#1579) |
| J13 | Package-repository scripts | `build-apt-repo.sh`, `build-yum-repo.sh`, `verify-packages-repo.sh`, `push-packages-repo.sh` | F1 | Not started |
| J14 | Crates from the LAN bazel-remote | nothing (the pool `stable / clippy` stops downloading crates from crates.io) | F1 | Done (#1519) |
| R1  | Toolchain checksums | `scripts/rust-toolchain-sha256s.sh` | F2 | Done (#1581) |
| R2  | Bazel clippy | `tools/ci/bazel-clippy.sh` and bazel.yml's `bazel clippy` step | F2, R1 | Done (#1582): removed, and its pin checks became a test |
| R3  | Bazel/Cargo target parity | — | — | Moved in review: now J11 |
| R4  | Sharded BDD runner | — | — | Moved in review: now J12 |
| R5  | Workspace discovery and the MSRV check | — | — | Moved in review: now in S1, as JavaScript; done there (#1589) |
| R6  | Pool routing copies stay identical | nothing (a new guard) | F1, F2 | Done (#1544) |
| R7  | ConformU install and parallel run, now JavaScript | `scripts/test-conformance.sh`, conformu.yml's install, its "Export CONFORMU_PATH" blocks and its parallel run | F1 | Done (#1591, #1593): the install action, the script deleted, and the parallel run an action |
| R8  | Packaging scripts a developer runs, one PR each | 8 scripts in `scripts/` | F2 | Not started |
| S1  | Sweep of the remaining inline blocks | the rest (listed in S1) | F1 or F2 | Done: bazel.yml (#1586), bazel-coverage.yml (#1587, #1588), the small blocks of nine more (#1589), check.yml's beta clippy (#1594), repin-bazel.yml's commit (#1595) and shellcheck.yml's install and lint (#1596) |
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
  by Bazel and Cargo. It sits
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

Done in #1564. The module is `.github/actions/lib/install.mjs`, and
[ci-logic.md](../skills/ci-logic.md#an-action-that-installs-a-tool) says
how an action uses it. A zip on Linux takes `unzip`, as the shell did,
rather than a zip reader of our own. The fallback to the previous week's
download, for a rolling pin, came with J5, its first user (`downloadAll`).
`install-omnisim`'s macOS quarantine step went: neither curl nor Node
marks a download as quarantined.

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

  Done in #1566. The script went, and with it the second table: a
  developer runs `bazel run //.github/actions:install -- <tool>`, which
  runs the actions' own modules on the Node `MODULE.bazel` pins
  (ADR-022 Amendment A, chosen over a Rust `cargo ci install` that would
  have reimplemented `lib/install.mjs`). It installs Pebble and OmniSim,
  which had no developer installer; J5 and J6 add the SDKs. It ships
  nothing and offers nothing from the private mirror.
  `install-omnisim`'s `repo` and `version` inputs moved into
  `omnisim.mjs`, beside their pins; no workflow set them.
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

  Done in #1573. The table is `install-astap/assets.json`, JSON because
  the refresh rewrites it: the workflow's three awk programs became the
  action's `pin` step, which also refuses a table that names another
  archive than the workflow's matrix, and its `repin` step, and a test
  keeps the file in the one layout `repin` writes. The curl flags became
  `download` options; the D05's rate floor (`minBytesPerSecond`), its
  bound on retries (`retryForMs`) and `--retry-all-errors` (`retryAll`, for
  a pool of mirrors) are new in `lib/install.mjs`, off by default. The download moved off `fetch` onto Node's own HTTP client:
  Node 24's `fetch` crashed the process on two of three legs of the PR's
  `plate-solver-smoke` run, when a SourceForge mirror closed the
  connection under a paused body (nodejs/undici#5360). The smoke and
  `plate-solver-smoke` also run on a change to `.github/actions/lib`, which
  the action runs. The macOS quarantine step went, as J2's did. The
  workflow's own D05 download, with its copy of the pin, stays for J8.
  ASTAP joined the developer's install in #1574, at the owner's ask,
  though Amendment A named only J5 and J6: `installAt` puts the CLI and
  the D05 database in one directory.
- **J5 `install-zwo-sdk`** (442 lines, bash and PowerShell). A sudo and a
  sudo-free Linux path, a macOS path and a Windows path; the cache restore
  and save are separate steps; the Windows download is the unpinned,
  weekly-cached one. `check-pkg-assets.sh` reads this action's `ref`
  default and Windows URLs, so they stay where it reads them or it
  changes in the same PR. The SDK also joins the developer's install
  (ADR-022 Amendment A, `bazel run //.github/actions:install`), which
  replaces the copy-paste block in `services/zwo-camera/README.md`.

  Done in #1575. The commit and the CDN links moved into `zwo-sdk.mjs`
  (`REF`, `CAMERA_SDK_URL`, `EFW_SDK_URL`, `EAF_SDK_URL`), beside the
  files they fetch, as J3 moved OmniSim's release: no workflow set the
  `ref`, `prefix` or URL inputs, and `check-pkg-assets.sh` now reads the
  three it checks from their lines there. The SDK still goes where
  libzwo-sys's build script and Bazel look: `/usr/local` with sudo on
  hosted Linux and macOS, `$RUNNER_TEMP/zwo-sdk` on the sudo-less Pi,
  `$RUNNER_TEMP\zwo-sdk\lib` on Windows, the very string, since Bazel
  passes `ZWO_SDK_LIB_DIR` to every Windows action. Every OS now restores
  and saves its cache in two steps and saves only a new download; the
  previous week's fallback is `lib/install.mjs`'s `downloadAll`, for any
  rolling pin. Two changes from the shell: on macOS a library is pointed
  at the copied libusb when `otool -L` shows it loads libusb by `@rpath`,
  where the shell ran `install_name_tool -change` on all three and
  ignored its failures; and on Windows each library is searched for in its
  own SDK, the first by path among the x64 builds, where the PowerShell
  searched all three SDKs and took the first file it listed. `makeRunnable`
  moved into `lib/install.mjs`, as #1566's review was told it would once a
  third module needed it. The developer's `zwo-sdk` installs into
  `/usr/local` on Linux and macOS, with sudo where the developer cannot
  write, and into `~/.local/opt/zwo-sdk` on Windows. It replaced the
  copy-paste blocks in `services/zwo-camera/README.md` and
  `crates/zwo-rs/README.md`, each with its own copy of the commit.
- **J6 `install-svbony-sdk`** (367 lines, bash and PowerShell). Windows
  downloads from the project's private mirror, against a SHA-256; Linux
  and macOS from the INDI repository, against a git ref.
  `check-pkg-assets.sh` reads the `ref` default here too. The INDI
  downloads join the developer's install; the Windows one does not, since
  it comes from the private mirror (ADR-018 §7), which developers are not
  given.

  Done in #1578. The steps became `svbony-sdk.mjs`, run by a composite of
  resolve, cache and install. The SDK goes where it went: `/usr/local`
  with sudo on hosted Linux and Intel macOS, `$RUNNER_TEMP/svbony-sdk/lib`
  on the sudo-less Pi. On Windows `$RUNNER_TEMP\svbony-sdk\lib` now holds
  the import library and DLL alone, where the step pointed into the
  unpacked zip; no Bazel action carries the string, since Bazel never
  links the SDK. The commit, the Windows version and its SHA-256 moved
  into the module from inputs no workflow set, and `check-pkg-assets.sh`
  reads `REF` there. The mirror's token goes through `download`'s new
  `headers`, which `get` drops on a redirect to another origin, as fetch
  does, and the zip still goes into no cache: a fork's pull request can
  restore the repository's caches. Two changes from the shell: the
  sudo-free path installs the library as the sudo path does, `.so.1` with
  a `.so` link; and on macOS libusb is copied and pointed at only when
  `otool -L` shows the library loads it by `@rpath`. `findRuntime`,
  `linkRuntime`, `loaderFinds`, `canWrite` and `unpackZips` moved from
  `zwo-sdk.mjs` into `lib/install.mjs`, now that two modules use them. The
  developer's `svbony-sdk` installs into `/usr/local` on Linux and Intel
  Macs, and on Windows says where to get the SDK; it replaced the
  instruction in `crates/svbony-rs/README.md` to install the library by
  hand.

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

Done in #1580, as the `refresh-astap` action: `detect/` and `publish/`,
one for each of the workflow's two jobs, with the bash, the two
`github-script` blocks and install-astap's `pin` and `repin` steps gone.

- **detect**, one job per runner as before. The matrix names the runner
  and whether to verify, and the action takes the archive, its folder and
  its binary from the table, so the workflow keeps no copy of them; the
  Windows-ARM64 job became a row with `verify: false`. The download is
  `lib/install.mjs`'s, which now resolves to the SHA-256 it computed,
  with the CLI's re-rolls of a bad mirror where curl tried three times.
  A rotated binary is checked as the shell checked it: its banner, M101
  with the same hints to within 0.01°, and the field with no stars. The
  centre is read from the `.wcs`'s 80-character cards, where grep matched
  the text, so a CRVAL1 inside a COMMENT card no longer counts.
- **The D05 database** is downloaded each time, with the hardened
  download options install-astap uses. The workflow's own cache entry and
  its copy of the pin went: over the last four rotations it hit once in
  fifteen checks, since an entry goes after a week unused and the refresh
  runs about monthly. A D05 that rotated too is still warned about and
  used.
- **publish** reads the results, then proposes the new pins or files the
  blocker, and comments on install-astap's tracking issue, as the shell
  did. It commits through the GitHub API (`git/trees`, `git/commits`,
  `git/refs`) instead of pushing from a checkout: it reads the table at
  main's tip, moves only the pins that changed (`repin`), and proposes
  nothing when main already pins what upstream serves.
- **Changes from the shell:**
  - Each job has only the permissions it needs. `detect`, which runs a
    binary nobody has reviewed, has `contents: read` and no token on
    disk. Before, every job had `contents: write`, `pull-requests: write`
    and `issues: write`, and checkout left the token in `.git/config`.
    The binary also runs without the runner's `ACTIONS_*` variables
    (`ACTIONS_RUNTIME_TOKEN` can write the repository's caches), which a
    JavaScript action gets and the shell's `run:` step did not. That is no
    sandbox: the action's own process keeps the token, which a child can
    read on Linux from `/proc/<ppid>/environ`, more easily than the
    shell's binary could have dug it out of the runner, and a hosted
    runner's user has passwordless `sudo` either way.
  - A solve still running after 120 s is killed and fails the check,
    where the shell would have held the job to its six-hour limit.
  - The blocker goes through `commentOrOpen`, as J7's tracking issues
    do, so a blocker opened without its label fails the step. The label
    `astap-refresh-blocked` did not exist in the repository, so the first
    blocker would have been opened without it, and every later failure
    would have opened another, silently; now each of them still opens one,
    but fails the step saying to create the label. The tracker's comment names the blocker,
    and a result's row names why its check failed.
  - `workflow_dispatch` takes `dry-run`: `publish` reads what it would
    act on and prints what it would write.
- **Shared:** the GitHub API client J7 wrote in `tracking-issue` moved
  to `lib/github.mjs`, now that two actions call the API, and `error`, an
  annotation that leaves the step to succeed, joined the helpers.
- **Proof:** `refresh_astap_test` runs a fake ASTAP, a Node process
  started in the binary's place that follows the scenario its archive
  holds, against the `.wcs` the real ASTAP CLI-2026.09.01 wrote for M101,
  and a fake GitHub API that answers in the shapes the real one gave. A
  mutation set of 49 changes turned every one red. The entry files ran
  for real on Linux x64 against SourceForge: with a pin set wrong, the
  real binary solved M101 and the result passed, in 5.5 s with the D05
  download; with the real pins there was no rotation, and the same held
  for Windows-ARM64 from Linux. `publish` ran a dry run against the real
  API, reading main's ref, commit and table. On the Windows VM the tests
  passed under Bazel, and the real `astap_cli.exe` (CLI-2026.09.19)
  passed its check on Node 24.20.0. On every hosted runner, from a
  temporary workflow on a throwaway branch that set each runner's pin
  wrong: the four with a job of their own solved M101 to RA 210.80988,
  Dec 54.34693 and failed the empty field, Windows-ARM64 was compared
  only, and `publish`'s dry runs read main with the job's own token, one
  finding main already pins what upstream serves, the other, given a
  pin main does not hold, reading the branch, the pull requests and the
  tracking issue. On macOS the first D05 attempt met a crawling mirror,
  was cut at the 300 s limit and the next finished in 4 s, where curl,
  with no limit, would have waited it out.
- **Writes GitHub fails:** the tree, the commit, moving the branch and a
  pull request's new body are sent again after a 5xx, since a repeat makes
  the same thing; creating the branch, opening the pull request and the
  comments are sent once, as a repeat could fail or make a second one.
- **After merge:** `publish`'s writes (the tree, commit, ref, pull
  request and comments) have only met the fake until the first rotation
  makes them; watch that run. A dispatch on main with `dry-run` proves
  the five `detect` jobs and `publish`'s reads.

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

Split in three PRs, each proven where it can be:

1. **The nightly's plan and its MSI job's fetch**, which a dispatch with
   `dry_run=true` runs.
2. **The nightly's publish**: the rpm asset names, the checksums, the
   tag, the prerelease and the tap's formulas. Tests against a fake API
   carry it, and the next nightly is watched.
3. **release.yml's tag-version check and its formula push**, on the
   second's tap code. Its first live run is a real release.

Done so far:

- **The plan and the fetch** (#1597), as the `packages/plan` and
  `packages/prior-msi` actions.
  - The plan reads the nightly tag through the API, where it ran
    `git ls-remote`, and peels an annotated tag through `git/tags`. The
    version's short commit is the commit's first seven hex digits, as
    `git rev-parse --short=7` gave them unless another object in the job's
    depth-1 clone shared them.
  - The fetch downloads the MSI through the API by the asset's id, into
    `RUNNER_TEMP`, and checks it against the SHA-256 GitHub gives for the
    asset, which `gh release download` did not. An asset with no SHA-256
    fails the leg, as an unreadable release does.
  - The plan's checkout keeps no token: nothing in the job pushes or
    reads the remote with git any more.
- **The publish** (#1598), as the `packages/nightly-assets`,
  `packages/publish-nightly` and `packages/tap` actions.
  - The rpms' names and the sums are as the shell made them. A kind of
    asset with none in the set fails the sums, as the shell's unmatched
    glob failed `sha256sum`.
  - The tag moves through the API, by a forced PATCH of its ref or a new
    ref, where the job pushed it from its checkout, which now keeps no
    token.
  - The release loses every old asset, listed page by page, and gets its
    notes, which are byte for byte the shell's for the same inputs. The
    new assets go up five at a time, as `gh release upload` sent them.
    An upload that fails transiently is retried, as `gh` did, but first
    whatever it left under its name is deleted, since GitHub refuses a
    second asset of one name; and the name GitHub gives an asset must be
    the one the sums list.
  - The tap gets one commit through the API, on its main, by
    github-actions[bot] as the push from a clone was, and moved to only
    as a fast-forward; a tree equal to the tip's commits nothing, where
    `git diff --cached --quiet` did. `HOMEBREW_TAP_TOKEN` reaches that
    step alone, where the clone kept it in its `.git/config` for the rest
    of the job. A step before the tag moves writes the commit's tree with
    it, which takes the same right, so a token that can no longer write
    the tap fails the job before anything is published, as the clone of
    the tap did. Its `stable` channel is tested here and wired into
    release.yml by the next PR.
  - `lib/github.mjs`'s `github` takes an `upload` body and any method,
    and its error carries the wait a rate limit asked for, which `retry`
    honours around an upload; `everyPage` follows a paged list, which
    `openIssues` now uses.
  - A dry run skips `publish`, so the tests against a fake API carry
    this, and the first nightly after merge is watched.

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

Done in #1585, as the `remote-cache` action, which each bazel.yml and
bazel-coverage.yml job runs once, after its checkout. The script and its
four `source` lines went, and every Bazel step adds `--config=ci-remote`
where it had `"${REMOTE_FLAGS[@]}"`.

- **The flags** are the `ci-remote` config, in `ci-remote.bazelrc` in the
  workspace root, which `.bazelrc` try-imports. It is a named config, not
  plain `build` lines, so a step run without the action fails on an
  undefined config instead of building with no cache. The venues are the
  script's: on the pool, the LAN bazel-remote over gRPC with its Remote
  Asset API, or over HTTP; elsewhere, the cloud cache; and
  `--remote_upload_local_results=false` for a job with no write
  credential.
- **The credential** reaches Bazel through `--credential_helper`, scoped
  to the cache's host, which Bazel 9.2.0's source settles:
  - Bazel runs the helper with the Bazel command's own environment
    (`CredentialHelper.java`), where the job's env block puts the
    credential on trusted events only.
  - It asks about the HTTP cache's base URL, and each gRPC service's URI,
    not each blob.
  - The server keeps an answer per URI for 30 minutes
    (`--credential_helper_cache_duration`). A job's Bazel commands share
    one environment, so this never carries a credential into a read-only
    command.
  - A helper scoped to a host is asked about no other, so the archives
    Bazel downloads never see it. The helper also answers only for the
    cache's host.

  The helper is configured on read-only jobs too, where it answers with no
  headers, so every pull request's legs start it as push to main does.
- **The launcher.** Bazel starts a helper as a program, and a pool
  runner's PATH may have no Node. So the action writes a launcher that
  names the runner's own Node: a shell script, and on Windows a `.cmd`
  file, which CreateProcess runs through cmd.exe.
- **`remote-cache-base`.** `build:remote-cache` keeps the cloud cache's
  URL, and its two other settings moved to `build:remote-cache-base`,
  which the pool venues take with the LAN URL. A config that set
  `--remote_cache` twice, once through `--config=remote-cache`, made
  Bazel warn in every step that two config expansions set the option. The
  script's second `--remote_cache`, on the command line, had raised no
  warning.
- **Masking.** The action masks the LAN cache's URL and host before
  anything prints, as the mask step does. The host is also in
  `--credential_helper`, which `--announce_rc` prints, so the HTTP venue's
  bare host is masked now too.
- **Proven locally** with Bazel 9.2.0:
  - Against a fake HTTP cache, every request (`GET /ac`, `PUT /ac`,
    `PUT /cas`) carried the helper's Authorization header. A job with no
    credential sent none and uploaded nothing.
  - Against a fake gRPC server, `GetCapabilities` carried it.
  - Against the cloud cache, Bazel asked the helper once.
- **What only main proves:**
  - the credential on each venue;
  - the cloud cache's venues, all on hosted runners that only push and
    the schedule use, the macOS leg among them;
  - the pull request's legs prove that Bazel starts the helper on the
    pool's Linux and Windows templates.

  With the credential and an `http://` LAN cache (the asset API off),
  Bazel prints "Credentials are transmitted in plaintext": the credential
  crossed the LAN in plaintext before too, unannounced. With the asset
  API on, the pool's cache is gRPC, and Bazel checks only `http://` URLs.

### J11 — Bazel/Cargo target parity

Dropped in #1583, on the owner's call. `scripts/check-bazel-cargo-parity.sh`
(82 lines), run by `parity.yml` alone (on push to main, nightly and on
dispatch), failed on a Cargo workspace member with no Bazel Rust rule in
its own directory. Bazel is the primary build: nothing new is compiled or
tested without its BUILD.bazel, so the check guarded a case that does not
arise. Its record agrees: in 400 runs from 2026-08-27 it found no gap, and
its one failure (2026-09-09) was `bazel query` itself exiting 7.

- **What went:** the script, and the workflow's step that ran it and the
  Rust toolchain step only that step needed.
- **What stays:** the workflow's two "Guard - rules_rust …" steps. The
  workflow is `rules-rust-patches.yml` now, its job `rules_rust coverage
  patches`, after what it still checks. S1 took the two steps in #1589:
  the `rules-rust-patches` action.

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

Done in #1579, as a JavaScript action: `bdd-sharded/bdd.mjs`, with the
bash gone. It runs what the shell ran: one build of every bdd target,
then rp as four shard processes, session-runner as two, polar-align and
filemonitor in one stream and the other 21 suites in one more, each
stream's output in a log of its own, printed as a group once they end.
The inputs and the six callers are unchanged.

- **No shell and no jq.** The action reads cargo's JSON itself and starts
  each binary with no shell between, so two of the shell's guards against
  itself went with it: the count of the suites each list ran (a loop that
  shared stdin with them had dropped suites) and the check that the split
  covered every binary (an awk that matched nothing). The tests check the
  split, and a suite's stdin is never the step's.
- **A stopped run.** The runner stops a cancelled step, one past its
  job's `timeout-minutes` included, with SIGINT, then SIGTERM 7.5 s
  later, then a kill 2.5 s after that, which on Linux and macOS reaches
  Node alone (actions/runner, `src/Runner.Sdk/ProcessInvoker.cs`). Either
  signal sends each running suite SIGTERM, then SIGKILL 5 s later, and
  the logs are printed before the kill, so a stream that hangs still
  shows what it printed. No further suite starts.
- **A stream that throws**, which no real suite does (spawn refusing its
  arguments), stops the others, and the step fails once every suite it
  started has ended.
- **Proof:** tests that start each fake suite as a Node process of its
  own, each shown to fail with its behaviour broken, and a run of the
  entry file on the real workspace. Dispatched on the branch: `test.yml`
  on hosted Linux, macOS and Windows, and `scheduled.yml`'s three BDD
  jobs, `updated` among them, which sets `rustflags`; and a `test.yml`
  run cancelled while the suites ran, whose logs show each stream's group.

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

Done in #1581, as `cargo ci rust-toolchain-sha256s <release>`
(`tools/rusty-photon-ci/src/rust_toolchain_sha256s.rs`), replacing
`scripts/rust-toolchain-sha256s.sh` (124 lines). pre-push.md sends
developers to it when a Rust pin moves: it prints MODULE.bazel's
`sha256s` entries from a release's channel manifest, after checking the
manifest's detached signature against the Rust release key, pinned by
fingerprint.

- **`gpg` still checks the signature.** rPGP (`pgp` 0.21.0, default
  features) resolves to 154 crates, 84 of them new to Cargo.lock, that
  every build of `//...` would compile and lint, for a tool run once a
  Rust release. The tool runs `gpg` with `--no-autostart`, since a
  signature check needs no gpg-agent, and with `--log-file`, so a good
  signature prints nothing but the entries and a refused one quotes gpg's
  log. `curl` downloads, as before. Each
  download is a `read`, so the tests answer it from a saved file, and gpg
  checks the files written from the text the entries are parsed from.
- **For the developer** nothing else changes: `curl` and `gpg` on the
  PATH, the same arguments, the same output. The first run compiles the
  crate.
- **The list for R2** is `Release::archives`: each archive's package,
  manifest target, file name and `sha256s` key. bazel-clippy.sh keeps its
  copy until R2, and its comment and its two errors name the tool.
- **Dependencies:** `toml` 1.1.8 joins the workspace's, reading only
  (`std`, `serde`, `parse`). Its lock entry raised `toml_datetime` to
  1.1.2 and `toml_parser` to 1.1.5, which `toml_edit` shares (through
  `proc-macro-crate`). `serde` and `tempfile` were the workspace's
  already. The tests' fake runner moved to `process.rs`, shared with
  repin-bazel-lock's.
- **Tests:** the channel manifests of 1.99.0 and nightly/2026-08-15, cut
  to the tables the tool reads (`testdata/`), give exactly the entries
  MODULE.bazel pins, and the nightly its comment line. Edited, they show
  the tool refusing a missing host, a missing `.tar.xz`, an archive named
  otherwise, a nightly of another date and text that is not TOML; with
  the fake's answers, a key of another fingerprint, a signature by another
  key, one gpg rejects, and a failed download or import.
- **Proven locally**, on 2026-10-20: against static.rust-lang.org, the
  tool printed what the script printed for both pins, byte for byte,
  the nightly's comment line on stderr included, starting no gpg-agent.
  With `curl` shimmed to serve local files to the real `gpg`, it
  refused a manifest with a line appended (gpg's "BAD signature", quoted),
  a key file holding another key (its fingerprint), and a manifest signed
  by a second key the key file also held: gpg reported a good signature,
  and VALIDSIG named the other key. A nightly date the server lacks fails
  with curl's 404.
- **Unproven:** a run on macOS or Windows. The tool runs on no CI venue;
  the Bazel legs run its tests on Linux and Windows. On Windows it needs
  `gpg` on the PATH (Gpg4win), which the bash script never ran on.

### R2 — Bazel clippy

Done in #1582, by removal rather than a port. The owner chose on
2026-10-20 to drop Bazel's clippy. `stable / clippy` runs Cargo's two
passes on the pool for every PR, so the `bazel clippy` step added one
thing: a lint of Windows-only code before a merge, in the Bazel targets'
feature sets.

- **What went:** `tools/ci/bazel-clippy.sh` (185 lines); bazel.yml's
  `bazel clippy` step on every leg, its job budget back to 40 minutes and
  the test steps to the default gating; `.bazelrc`'s `--config=clippy`
  and its two `build` settings; and `clippy.toml`'s `msrv`, which only
  Bazel's clippy read. Cargo's clippy takes the MSRV from each member's
  `rust-version`, which every member inherits. Every Rust target's
  `lint_config` went too, with the 56 `extract_cargo_lints` targets that
  fed it (below).
- **What the step cost**, over the 40 pool PR runs before the decision:
  1–3 s on Linux and 5–7 s on Windows when no Rust changed, 23–45 s and
  27–61 s when it did, and 35–45 s and 50–105 s for a dependency bump. On
  the macOS leg (push to main) it took 8–23 s, or 126–167 s after a
  dependency change.
- **What is lost:** a clippy lint in Windows-only code surfaces after a
  merge, from check.yml's `windows / clippy` on the push to main or
  overnight. The Windows leg still compiles that code on every PR with
  `-Dwarnings`, so a compile error or a rustc warning still fails the PR.
  Bazel's per-target feature sets linted no Linux code Cargo's two passes
  miss: the one test module compiled only with a feature off
  (svbony-camera's `production_default_tests`) is linted by neither.
  macOS code was already linted only after a merge, by both.
- **What moved:** the script's checks of the toolchain pins (the stable
  pin against the MSRV, the coverage nightly's cycle, `sha256s` against
  `Release::archives`) are a test in the tools crate, `toolchain_pins.rs`,
  which `bazel test //...` runs on every leg. It reads `MODULE.bazel`'s
  `rust.toolchain` line by line, and fails on a line of any other shape
  inside `versions` and `sha256s`. So nothing has to launch a tool inside
  the Bazel legs, which was this item's open question. The script's
  checks of the lint setup (`lint_config` on every target, no opt-out
  tags) served only Bazel's clippy and went with it.
- **`lint_config` went, on the owner's call.** Without clippy it gave
  Bazel's rustc one flag, `--warn=unexpected_cfgs`. That is the lint's
  default level, and it never fires under Bazel, which passes rustc no
  `--check-cfg`. A rustc lint added to `[workspace.lints]` later reaches
  Cargo only (pre-push.md, *Bazel's Rust pin*).
- **The cache:** removing the two `build` settings changed no other
  action. All 1,062 Rust actions of `//...` and its dependencies kept
  their keys and output paths (`bazel aquery`, before and after).
  Removing `lint_config` changes the keys of the 253 first-party `Rustc`
  actions, whose command line loses the lint file, so each leg's first
  build after the merge recompiles the workspace's own crates once. Every
  third-party action keeps its key. The 65 actions that read the lints,
  or built the tool that does, no longer run.
- **Proven locally:** the test passes against the real files, and fails,
  naming the mismatch, against each of a stable pin bumped alone, an MSRV
  lowered alone, and a nightly date bumped without its comment line. 21
  mutations of the checks and the reader all turn it red.
- **Unproven until main:** the macOS leg without the step, on the first
  push after the merge.

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
the tools crate. They are in S1's list, and S1 took them in #1589: the
`cargo-workspace` action.

### R6 — Pool routing copies stay identical

Done in #1544, as a test in `tools/rusty-photon-ci`. It reads the
workflows and `.github/actionlint.yaml` with a YAML parser. It started by
reading them line by line, and every review of that reader, in #1547 and
#1548 too, found YAML it read otherwise than GitHub.

The pool's `runs-on` routing expression is copied into bazel.yml,
bazel-coverage.yml, repin-bazel.yml and check.yml. ADR-020 makes it a
security boundary, and until R6 only a comment kept the copies in step.
They come in two shapes: bazel.yml holds the Linux and Windows clauses in
one expression, each prefixed `matrix.os == '…' &&`, with `|| matrix.os`
as the fallback, while the others are standalone and end in
`|| 'ubuntu-latest'`. A PR that changes one copy now fails
`bazel test //...`, and a PR that changes them all has to change the test
too, in the same diff the reviewer reads. Raised by the review of #1505.

- **The guard:** `tools/rusty-photon-ci/src/pool_routing.rs`, a module
  only the crate's tests compile. Its `RUNNERS` lists every job whose
  `runs-on` names a self-hosted runner, by workflow and job id:
  - the four copies, each with its reviewed branches (the kill-switch
    variable, the trusted-event test and the pool labels, behind a
    `matrix.os` label in bazel.yml) and its hosted fallback;
  - the three label lists, in pi-nightly.yml and proxmox-runner-test.yml.
- **What it checks.** It reads every workflow and
  `.github/actionlint.yaml` with a YAML 1.2 parser (`saphyr`), so quoting,
  escapes, folding, block scalars, comments and aliases read as they do
  for GitHub. It reads a file as GitHub does or not at all. What it would
  read otherwise fails the test, naming its line:
  - a tag of any kind, which no workflow needs: the parser drops some,
    such as `!!str` on a list, so the test could not say how it reads one;
  - a key that is not a string;
  - two keys that differ only in case, a pair GitHub refuses;
  - a `<<` key, a key to GitHub and a merge to actionlint's parser;
  - other than one document.

  A job's `runs-on` names a self-hosted runner when a string in it
  mentions `RP_POOL_`, picks a runner group or names one of the labels:
  `self-hosted`, and our runners' own (the pool's two and the Raspberry
  Pi's `raspberry-pi`). It reads our runners' labels from
  `.github/actionlint.yaml`, the list actionlint checks `runs-on` against,
  so a new runner label goes there and nowhere else. Each must be a plain
  string (letters, digits, `.`, `_`, `-`), or the test fails naming it.
  Nor may one be a label that YAML reads as other than a string where a
  workflow writes it unquoted, in any case: `2024`, or `tRUE` written
  `true`. The test reads only a workflow's strings, so it would not see
  one. Labels are matched in any case, as GitHub matches them, and only as
  whole labels, so a file name such as `raspberry-pi-runner.md` is not
  one. Keys are matched in any case too, so the test reads at least the
  keys GitHub does.
  - **Each copy** is split at its top-level `||` into pool branches and a
    fallback, and each must equal the reviewed one. Whitespace is collapsed
    first, so a reflowed copy still matches. The fallback is checked too,
    which the plan had left out: a copy that fell back to a pool label
    would send every falsy branch to the pool, a fork PR among them.
  - **Each label list** must equal the reviewed one, label for label, in a
    workflow that only a schedule or a dispatch starts; another trigger
    fails the test.
  - **A new `runs-on` that names a self-hosted runner**, in any job of any
    workflow, fails the test until `RUNNERS` lists it by workflow and job
    id. A `runs-on` that picks a runner group (`group:`) counts as one:
    this organization's only group is the default one, which holds the
    pool's runners.
  - **A self-hosted label anywhere else**, in any key or value, fails it,
    so a matrix cannot carry one into `matrix.os`. That includes a block
    scalar's text, a string spelled with an escape, an anchor's value, and
    a job's name: the Pi nightly's job is `pi / linux / arm64 / stable` for
    that reason. Comments are not read. The message names the line of the
    file that names the label, or the line its string starts on when only
    an escape spells it.
  - **What it does not read fails it:** a file that does not parse, the
    refusals above, and a copy that is not one `${{ }}` expression. A
    workflow it does not read fails with that message alone: the runners
    `RUNNERS` lists in it are not reported missing.
- **Under Bazel** the workflows and `.github/actionlint.yaml` are the
  unit test's `data` (`//:workflows`, a glob in the root package, and the
  file the root package exports, since neither directory has a BUILD file;
  F1 took `.github` out of `.bazelignore`). The BUILD file sets
  `CARGO_MANIFEST_DIR` for the test, relative to the runfiles root; Cargo
  sets it when it runs a test.
- **The parser** is a dev-dependency of the tools crate, so only its tests
  build it, and its version is pinned exactly: its 0.x API still changes
  in minor releases. Of the Rust YAML crates compared, it alone is both
  maintained and keeps a line for every node, which the test's messages
  name. Like YamlDotNet, the parser in GitHub's runner, it reads `on` as a
  string, an alias as its anchor's value, `<<` as a key, and a duplicate
  key as an error. Its event parser, `saphyr-parser`, is a dev-dependency
  under the same pin: its events carry every tag, where the nodes `saphyr`
  builds keep only some.
- **Proven locally.** Each of these edits to the real files failed the
  test under Cargo, each file restored after its run:
  - the copies: dropping the same-repo test from check.yml; a pool-label
    fallback in repin-bazel.yml; the Windows kill switch in
    bazel-coverage.yml; bazel.yml's Windows leg trusting every pull
    request, and behind `macos-latest`; a line starting with `#` inside
    check.yml's copy;
  - as test.yml's `runs-on`: a new pool route, on one line or continued on
    a second; generic self-hosted labels; a label list;
    `[Self-Hosted, Proxmox-Ephemeral]`; `raspberry-pi` alone;
    `group: Default`; `self-hosted` spelled with a YAML escape; the same
    under a `Runs-On` key; `!!seq` on a label list;
  - check.yml's `msrv` job on an alias of `clippy`'s `runs-on`;
  - a quoted `runs-on` key in check.yml, with the same-repo test dropped;
    a custom tag, or `!!str`, before check.yml's block indicator, the
    custom tag failing with its refusal alone; `runs-on` and `Runs-On` in
    one job; a second document in test.yml;
  - in bazel.yml's matrix: `self-hosted`, `Self-Hosted`, `raspberry-pi`,
    and a line starting with `#` that names a label; in test.yml's `env`,
    a label on a line starting with `#` inside a multi-line quoted string,
    and one on a block scalar's third line, named at that line;
  - a `pull_request` trigger in pi-nightly.yml;
  - in actionlint.yaml: no `self-hosted-runner` key; a new label, plain, in
    double or single quotes, before a tab and a comment, or spelled with an
    escape, then routed as test.yml's `runs-on` or put in bazel.yml's
    matrix; a label with `#` inside its quotes; a `<<` key; the labels
    `"2024"`, `tRUE` and `0X10`;
  - in actionlint.yaml, ahead of the last label, with that label routed as
    test.yml's `runs-on`, the spellings reviews of the line-by-line reader
    found: a dash and a tab, a bare dash, a folded label, a line at
    `labels:`'s indent, an entry at column 0 with or without a colon, and
    a key between two entries.

  Reflowing check.yml's copy onto fewer lines passed, and so did blank
  lines after it and a quoted `runs-on` key: GitHub reads each the same.
  Breaking each rule of the reader in turn failed a unit test each time.
- **Not guarded.** The test catches a copy that drifts and a new route,
  in review; a route disguised on purpose gets past it. Against a fork's
  pull request the defence remains the approval its workflows wait for
  (ADR-020 layer 2). What gets past it:
  - a runner chosen without naming one of those labels, since a
    self-hosted runner also carries `Linux` and `X64`;
  - a label or runner group that reaches `runs-on` through a matrix or
    another value without naming one of those labels;
  - a label built at runtime, by `format()` or `join()`, or spelled with an
    escape inside an expression's string, such as JSON's inside `fromJSON`;
  - a runner chosen by a value from outside the workflow file, such as a
    variable or a script's output.

  The only difference found between the two parsers goes the safe way:
  `saphyr` reads a dash followed by a tab as a list entry, where YamlDotNet
  refuses the file.
- **Done when** the test runs on both Bazel legs of a PR, a workflow that
  differs from what it reviews fails it, and the workflows' comments name
  it.

### R7 — ConformU install and parallel run, now JavaScript

`scripts/test-conformance.sh` (450 lines) had two halves. Its
`--install-conformu`, which the README, hardware-validation.md,
pre-push.md and filemonitor's tests README sent developers to, installed
the latest ConformU into `~/tools/conformu`, on Linux x64 only and
unchecked. Its run tested filemonitor alone, with a config of its own:
what filemonitor's `conformu_integration` test, the command its
`[package.metadata.conformu]` gives, already does, and what conformu.yml
runs. CI installed ConformU with `ivonnyssen/conformu-install@v3`, also
unchecked, and two "Export CONFORMU_PATH" blocks found it on the PATH.

ADR-022's Amendment A makes a developer's install the CI install action's
own module, so once the filemonitor run goes nothing here is a tool a
developer runs, and the item is JavaScript. The owner chose on 2026-10-21
to replace both installs with a local action, and that it:

- stays on the latest release, as hardware-validation.md's version gate
  requires;
- checks each download against the SHA-256 GitHub's release API gives
  for the asset, and refuses a release without one. ConformU's releases
  carry one from v4.2.1 on; older ones are not supported;
- keeps no way to install an older release, since no old run has been
  reproduced;
- installs on every platform a developer uses, Windows by ConformU's own
  installer, since a developer has admin rights on their machine;
- installs a developer's copy into `~/.local/opt/conformu`, as the other
  tools do;

and that the script's filemonitor-only run is deleted, not ported. Two
PRs:

1. **The install** (#1591): `install-conformu`, which conformu.yml's two
   jobs use, exporting CONFORMU_PATH itself, so the Export blocks go;
   `install-conformu.yml`, which installs it on the four runners its
   table has whenever it changes; the developer's
   `bazel run //.github/actions:install -- conformu`; and the script,
   deleted.
2. **The parallel run** (#1593): conformu.yml's "Run ConformU tests
   (parallel)" block, as `conformu-run`. It runs each service's command
   without a shell, as the words it splits into, and refuses one a shell
   would read otherwise, and prints each suite's log as a group once they
   all end, where the block prefixed every line with the service's name
   as it came. `bdd-sharded`'s runner moved to `lib/processes.mjs`, which
   both use, and now stops each program's process group: SIGTERM ends
   `cargo test` and leaves its test binary running. The Windows jobs'
   "Run ConformU test" is one command, and stays YAML.

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
PRs, in any order; the owner chose on 2026-10-21 to take the smallest
together, in one (#1589):

- bazel.yml: the Windows profile flags chosen in `bazel build`,
  `bazel test (fast)` and `bazel test (BDD)`, now that J10 has taken
  their sourcing (done, below);
- bazel-coverage.yml: "bazel coverage" (its timeout and exit-status
  handling), "Annotate uncovered diff lines", "Capture bazel test logs",
  "Split combined lcov per package" (done, below);
- check.yml: "Verify workspace MSRV" (from R5, done, below); `clippy-beta`'s
  report and aggregation blocks, and its per-lint census `github-script`,
  which J7 left out (done, below);
- conformu.yml: "Discover ConformU services" (from R5, done, below); the two "Export
  CONFORMU_PATH" blocks and "Run ConformU tests (parallel)", which R7
  took (#1591, #1593);
- install-astap.yml: "Smoke test (Linux/macOS)" and "Smoke test
  (Windows)", a bash and PowerShell pair (done, below);
- rules-rust-patches.yml (`parity.yml` until J11): the two "Guard -
  rules_rust …" steps (done, below);
- pi-nightly.yml: "Symlink libusb for the QHYCCD static link" (done,
  below);
- plate-solver-smoke.yml: "Assert solve-time budget", "Log per-scenario
  timing" (done, below);
- release.yml: "Strip binary" (done, below);
- repin-bazel.yml: "Skip if already in sync", "Commit and push repin"
  (done, below);
- safety.yml: "Enable opt-level 1 for sanitizers", and "cargo test"'s
  flags per sanitizer (done, below);
- scheduled.yml: "Find Miri services" (from R5, done, below);
- shellcheck.yml: "Install pinned shellcheck" and its script list. They were
  check.yml's until the owner chose on 2026-10-21 to give the job a workflow
  of its own, which runs only when a file it lints changes: ADR-022 leaves
  that shell in bash, so the job keeps its work after this plan (done,
  below).

The "Mask runner-provided cache endpoint" steps stay in YAML for the
reason J10 gives.

Done so far:

- **bazel.yml** (#1586): each of the three Bazel steps chose its Windows
  profile flags with a bash array and an `if` on `RUNNER_OS`. An
  expression now picks them, `${{ runner.os == 'Windows' && '…' || '' }}`,
  so each step is one command again. The matrix is a plain list of OS
  names, which ADR-020's routing expressions read, so a matrix value
  would have meant changing them. Each step's Bazel arguments are the
  same as before, on every OS.
- **bazel-coverage.yml's run** (#1587): the `bazel-coverage` action runs
  `bazel coverage` with the step's flags. It replaces three steps' shell:
  - **"bazel coverage"**: it keeps the 50-minute cap the step had from
    `timeout`. Past it, Bazel is sent SIGINT, and SIGKILL 2 minutes later,
    and the runner's stop signal sends the SIGINT at once. It names a
    failure from Bazel's exit code, as the step did, in an annotation
    without the step's titles, which the helper module does not write.
  - **"Capture bazel test logs"**: gone. The upload reads
    `bazel-out/*/testlogs` itself, as bazel.yml's BDD log upload does, so
    the artifact's paths now start at the configuration directory
    (`k8-fastbuild/testlogs/…`). The step's rp:bdd tail had printed
    nothing since rp:bdd was sharded: it looked for the unsharded
    `services/rp/bdd/test.log`.
  - **"Split combined lcov per package"**: the action finds the combined
    report and fails on a missing or empty one, and gives its path as the
    `report` output. The step runs the two Python scripts on it.

  Bazel gets the step's environment without the runner's `ACTIONS_*` and
  `INPUT_*` variables, which a `run:` step does not have
  (`withoutRunnerVariables`, now in the helper module).
- **bazel-coverage.yml's "Annotate uncovered diff lines"** (#1588): the
  `uncovered-diff-lines` action installs diff-cover with the step's pip
  command and pin, fetches the PR's diff, runs diff-cover and runs
  `post_check_annotations.py`, each program with the environment the step
  gave it and only the poster with the token. The owner chose on
  2026-10-21 to keep the check required, so a diff that cannot be scored
  now gets the check, neutral and "Not scored", naming the stage that
  failed, where the step used to post nothing and leave the PR blocked.
  The poster exits 1 when it cannot create the check run, so the action
  knows to post it.
- **The small blocks of nine workflows** (#1589):
  - **R5's three**, which read `cargo metadata | jq`: the
    `cargo-workspace` action. Its `packages` step outputs the packages a
    `[package.metadata.<table>]` gives a command, as conformu.yml's
    "Discover ConformU services" and scheduled.yml's "Find Miri services"
    did, and its `msrv` step runs check.yml's `cargo msrv verify` per
    member, stopping at the first that fails, as the loop did. Against
    this workspace it finds what the jq found, byte for byte: 14 ConformU
    services, 5 Miri ones, 56 members to verify. A `command` that is not
    a command line now fails the job, naming the package, where jq passed
    it on to the matrix.
  - **install-astap.yml's smoke pair**: one step on every leg, the
    `install-astap/smoke` action, with both shells' checks. Node checks no
    execute bit on Windows, so there it checks the file exists, as the
    PowerShell did.
  - **rules-rust-patches.yml's two guards**: one step, the
    `rules-rust-patches` action. It fetches rules_rust once, checks both
    patches and reports each that is not effective, where the first
    failing step skipped the second.
  - **pi-nightly.yml's libusb link**: the `qhyccd-libusb` action, on
    `lib/install.mjs`'s `linkRuntime`, which the ZWO and SVBony SDK
    actions use for the same link. The Pi runs main alone, so its first
    nightly after the merge is the proof.
  - **plate-solver-smoke.yml's timing steps**: one step, the
    `plate-solver-times` action, which logs the times however the BDD run
    ended and checks the budget only after it passed, as the two steps'
    conditions did. The budget's numbers and their baselines are
    constants in its module. It reads a last row with no line end, which
    `read` dropped, and a Windows line end.
  - **release.yml's "Strip binary"**: the matrix names each target's
    `strip`, so the step is one command.
  - **safety.yml**: `CARGO_PROFILE_DEV_OPT_LEVEL=1` builds workspace code
    at opt-level 1 instead of a `sed` of Cargo.toml, and the targets
    `cargo test` takes per sanitizer are a matrix value.

  Two helpers came with them: `execute`, moved from
  `uncovered-diff-lines` now that `cargo-workspace` and
  `rules-rust-patches` also run a program as the `run:` step did, and
  `notice`.
- **check.yml's beta clippy** (#1594): the `beta-clippy` action, in the
  job's two steps.
  - **`report`** takes the report and aggregation steps: the stable
    gate's two clippy passes on beta, with every lint capped at a
    warning, then what they reported, printed, then
    `tools/ci/beta_clippy_census.py` on it, its table added to the run's
    summary. A pass that fails, as on a compile error, fails the step,
    naming the pass, and makes no census, as before. The files go in
    `RUNNER_TEMP`, not the checkout. The census script stays Python:
    ADR-022 leaves the Python scripts where they are, and developers run
    this one too (pre-push.md).
  - **`track`** takes the `github-script`, through `lib/github.mjs`: the
    issue per lint, its body as before, and the cap of 20 lints. It reads
    the label and creates it when the repository lacks it, where the
    script created it and took a 422 to mean it existed, and the issue
    list it reads leaves pull requests out.
- **repin-bazel.yml's commit** (#1595): the `commit-repin` action
  commits the refreshed lock through the GitHub API, as ci-logic.md has a
  change to the repository go, where the step committed in the clone and
  pushed with a git credential helper. It writes the lock as a blob, a
  tree on the checked-out commit's and a commit on it, by
  github-actions[bot] as before, and moves the branch to it as a
  fast-forward, so a branch that moved meanwhile fails the step, as the
  push did. The token is the step's input, the Dependabot secret or the
  job's own, as before.
  - **"Skip if already in sync" is gone.** The action compares the lock's
    blob with the one the checked-out commit holds and commits nothing
    when they match. "Validate lock resolves" now runs either way, which
    costs one analysis on a re-run after the commit landed, the one case
    where the lock is in sync.
  - **Proven after merge:** the job runs on dependabot PRs alone, so its
    first run is on the next dependabot cargo PR. Its tests run the
    action against a fake API, and its reads were checked against
    GitHub's (the root tree's entry for the lock, and its blob's SHA-1).
- **shellcheck.yml's install and lint** (#1596): the owner chose on
  2026-10-21 to move them too, though the job serves one workflow. The
  `shellcheck` action downloads the pinned release through
  `lib/install.mjs`, checks its SHA-256 and the version it reports, and
  runs it by its path from `RUNNER_TEMP` over the scripts it lists, the
  same 11 the `find` listed. Run by its path, no other shellcheck on the
  PATH can lint in its place, so the step's check of which one PATH
  resolved goes. It caches nothing, as the step did not; the job runs
  only when a file it lints changes. The pin and its reason move from the
  workflow's `env` to the action's module, and the workflow's paths
  filter takes the action and the helpers it imports. An empty script
  list is now an error rather than shellcheck's usage message.

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
  same PR: ADR-022's Consequences, F2's note on `repin-bazel.yml`, J1's
  scope bullet, and the comment above check.yml's `stable / clippy` job.
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

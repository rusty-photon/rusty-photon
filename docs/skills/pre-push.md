# Skill: Pre-Push Quality Gates

## When to Read This

- Before pushing a branch to the remote repository
- Before creating a pull request
- When you need to run CI checks locally to diagnose a failure

## Prerequisites

### Required tools

| Tool | Install | Used by |
|------|---------|---------|
| Rust stable | `rustup default stable` | All checks |
| cargo-nextest | `cargo install cargo-nextest` or `curl -LsSf https://get.nexte.st/latest/linux \| tar zxf - -C ~/.cargo/bin` | Test execution |
| cargo-hack | `cargo install cargo-hack` | Feature powerset checks |
| Docker | [docs.docker.com](https://docs.docker.com/get-docker/) | act-based workflow execution |
| act | `curl -s https://raw.githubusercontent.com/nektos/act/master/install.sh \| sudo bash` | Local CI runner |

### Optional tools

| Tool | Install | Used by |
|------|---------|---------|
| Rust beta | `rustup toolchain install beta` | Beta clippy, beta tests |
| Rust nightly | `rustup toolchain install nightly` | Sanitizers, miri |
| miri component | `rustup +nightly component add miri` | Miri checks |
| cargo-msrv | `cargo install cargo-msrv` | MSRV verification |
| cargo-llvm-cov | `cargo install cargo-llvm-cov` | Local ad-hoc cargo coverage (CI coverage is `bazel coverage`) |
| ConformU | [ivonnyssen/conformu-install](https://github.com/ivonnyssen/conformu-install) | Conformance tests |
| jq | `sudo apt install jq` / `brew install jq` | ConformU & miri discovery |
| llvm | `sudo apt install llvm` | Address sanitizer symbolization |

---

## Procedure

> **Bazel is the per-PR gate.** The required checks are
> `bazel / {ubuntu,windows}-latest` (build + test), `bazel coverage`, and
> the Cargo `stable / fmt` + `stable / clippy` lint jobs (Bazel does not run
> rustfmt/clippy). `bazel / macos-latest` is **not** a PR check — it runs on
> push-to-main and nightly, so a macOS-only break is caught on main within
> minutes of the merge rather than blocking the PR. `bazel/cargo target
> parity`, plus the Cargo build / test / hack / msrv jobs, run nightly and do
> not gate PRs — none of them collects coverage; `bazel coverage` is the sole
> source. So the authoritative pre-push is:
>
> ```bash
> bazel build //... && bazel test //...                       # bazel / <os> (build + tests incl. BDD; OmniSim suites need OMNISIM_PATH/OMNISIM_DIR)
> bazel coverage --config=coverage //...                      # bazel coverage (needs OmniSim)
> cargo fmt --check                                           # `stable / fmt`
> cargo clippy --all-targets --all-features -- -D warnings    # `stable / clippy` pass 1
> cargo clippy --workspace --lib --bins -- -D warnings        # `stable / clippy` pass 2 — default-features complement (#988)
> ```
>
> `bazel coverage` produces a report, not a verdict. To find out whether the
> lines you are about to push are actually tested — which is what the
> `codecov/patch` check gates on — see [coverage.md](coverage.md).
>
> The first two commands ARE the fast local inner loop: Bazel rebuilds/retests only
> the targets your change affects, backed by the local `--disk_cache` (see "Change
> detection" below). cargo-rail is **retired**. The `act` / raw-cargo steps
> below reproduce the **nightly** Cargo safety net when you need it.

### Step 1: Run the full CI suite via `act`

`act` executes the actual GitHub Actions workflows in Docker containers. Use it
to reproduce the nightly Cargo safety net (and the PR `fmt`/`clippy` lint jobs)
locally.

```bash
# Run all independent checks in parallel
act -W .github/workflows/check.yml -j fmt &
act -W .github/workflows/check.yml -j clippy &
act -W .github/workflows/check.yml -j hack &
act -W .github/workflows/check.yml -j msrv &
act -W .github/workflows/test.yml -j required &
act -W .github/workflows/safety.yml -j sanitizers &
wait

# Optional: rolling jobs (these only run on main/scheduled, not PRs)
act -W .github/workflows/scheduled.yml -j nightly &
act -W .github/workflows/scheduled.yml -j beta &
act -W .github/workflows/scheduled.yml -j update &
wait
act -W .github/workflows/scheduled.yml -j discover-miri -j miri  # slow
# conformu.yml only triggers on schedule/workflow_dispatch, so act needs
# the workflow_dispatch event explicitly:
act workflow_dispatch -W .github/workflows/conformu.yml -j plan -j conformu  # nightly + on-demand
```

> **Note:** `act` runs Linux Docker containers, so the macOS/Windows jobs
> (`test.yml` `macos` / `windows`) are skipped locally. Multi-OS `conformu` jobs
> run the ubuntu variant only.

### Step 2 (fallback): Raw `cargo` commands

When Docker or `act` is unavailable, use these cargo commands directly.

With `cargo-hack`:

```bash
cargo fmt --check
cargo hack --feature-powerset clippy --all-targets -- -D warnings
cargo hack --feature-powerset check
cargo nextest run --locked --all-features --all-targets
cargo test --locked --all-features --test bdd
cargo test --locked --all-features --doc
```

Without `cargo-hack`:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --workspace --lib --bins -- -D warnings
cargo nextest run --locked --all-features --all-targets
cargo test --locked --all-features --test bdd
cargo test --locked --all-features --doc
```

---

## Change detection: Bazel's action graph

Change detection is automatic. Bazel's content-addressed action graph rebuilds and
retests only the targets your change affects; everything else is a cache hit — the
local `--disk_cache` (`~/.cache/bazel-disk-cache`) plus the output base locally, the
remote cache in CI. So the local inner loop is simply:

```bash
bazel build //... && bazel test //...
```

There is no separate narrowing step to run. The nightly Cargo safety net always runs
the full `--workspace`. cargo-rail (the former local affected-package narrowing tool)
is **retired**; we accept losing its dep-hygiene (workspace feature unification,
unused-dep / dead-feature pruning, MSRV preview) because feature-unification breakage
still surfaces in the nightly `--workspace --all-features` build and the off-PR
`cargo hack --feature-powerset` job, and MSRV in the `msrv` / `publish-readiness` jobs.

---

## Detailed Workflow Breakdown

### check.yml

`fmt` and stable `clippy` run on every PR + push to main (required PR gates,
because Bazel does not run rustfmt/clippy). The `clippy-os`, `hack`, and `msrv`
jobs run on push to main, the nightly schedule, and `workflow_dispatch` —
skipped on PRs via `if: github.event_name != 'pull_request'`. ("Off-PR" below =
that set.) `clippy (beta)` is narrower still: schedule and `workflow_dispatch`
only, since only the scheduled run acts on its census. Stable `clippy` also
asserts — before linting — that the dual-homed manifests' concrete `[lints]`
copies match `[workspace.lints]` (`python3 tools/ci/check_lints_parity.py`,
runnable locally with no arguments; docs/plans/workspace-lints.md §L7).

| CI Job | Local Command | Prerequisites | Runs |
|--------|---------------|---------------|------|
| **fmt** | `cargo fmt --check` | stable rustfmt | **PR gate** |
| **clippy (stable)** | `cargo clippy --all-targets --all-features -- -D warnings` then `cargo clippy --workspace --lib --bins -- -D warnings` (#988) | stable clippy | **PR gate** |
| **clippy-os (windows / macos)** | same two passes, on that host OS | stable clippy | Off-PR |
| **clippy (beta)** | `cargo +beta clippy --all-targets --all-features -- --cap-lints warn` | beta toolchain | Nightly |
| **hack** | `cargo hack --feature-powerset check` | cargo-hack | Off-PR |
| **msrv** | `cargo msrv verify` | cargo-msrv | Off-PR |

**`clippy-os` is the only clippy in CI that compiles OS-gated code.** The
required stable gate runs on ubuntu, so `#[cfg(windows)]` / `#[cfg(target_os =
"macos")]` production code is outside it — and Bazel cannot substitute (it runs
no clippy, and its `-Dwarnings` is rustc's set, which never evaluates
`clippy::` tool lints). The `windows / clippy` + `macos / clippy` legs close
that hole off-PR (#984): a violation lands on main and surfaces within minutes
of the merge (push) or overnight (schedule, via the `check-nightly` tracking
issue) rather than failing only a Windows/macOS contributor's pre-commit hook.
**Every clippy job runs two passes** (#988): `--all-features` turns `mock` /
`simulation` ON and thereby cfgs OUT the real-hardware production slices behind
`not(feature = "mock")` / `not(feature = "simulation")`. The second pass —
`cargo clippy --workspace --lib --bins -- -D warnings` — compiles each crate's
*default* feature set (empty everywhere except zwo-rs/libzwo-sys, whose
defaults are their device features, still sim-off), turning those slices ON; on
the windows leg that is what lints `#[cfg(all(windows, not(feature =
"simulation")))]`, qhy-camera's delay-load DLL machinery. The pass is `--lib
--bins`, **not** `--all-targets`: building dev targets pulls in dev-dependency
edges (four camera/focuser services force `simulation` onto their FFI wrapper;
`rp` forces `mock` onto rp-guider/rp-plate-solver) and resolver feature
unification would re-enable those features for five crates, silently un-linting
the complement. The accepted residuals: test-target code behind `not(feature =
...)` is compiled by neither pass, and a cfg mixing feature-on with feature-off
escapes both (the only ones today are zwo-rs's `all(not(simulation),
any(camera|efw|focuser))` gates — covered, since zwo-rs's defaults enable the
device features; a future one elsewhere still compiles under the nightly `hack`
powerset, rustc-only, and a deny-set widening should re-scan cfgs paren-aware
rather than by line grep).

**Stable and beta clippy answer different questions.** Stable is the gate and
must be silent (the `clippy-os` legs extend the same stable deny set to the
other two OSes off-PR). Beta is a heads-up and must never block: `--cap-lints warn`
downgrades every lint — including the ones `[workspace.lints.clippy]` denies —
so an upstream release that adds a lint to a denied group reports instead of
turning the nightly red. Because stable is green on `main` by construction,
every finding beta reports is new on the beta channel, with no set-differencing
needed.

The scheduled beta run aggregates its findings per lint with
[`tools/ci/beta_clippy_census.py`](../../tools/ci/beta_clippy_census.py) and
keeps one `beta-clippy`-labeled issue per lint — opened on first sighting, body
rewritten each night, closed automatically once the lint stops firing. To
reproduce the census locally:

```bash
cargo +beta clippy --all-targets --all-features --message-format=json \
  -- --cap-lints warn > /tmp/clippy.json
python3 tools/ci/beta_clippy_census.py --root "$PWD" --summary /tmp/census.md \
  < /tmp/clippy.json > /tmp/census.json
```

`--all-targets` compiles each source file once per target, so the script
deduplicates sites on (lint, file, line, column); the raw JSON over-counts.

The workspace uses a single MSRV (currently 1.94.1) declared in the root
`Cargo.toml` via `[workspace.package]`. All members inherit it with
`rust-version.workspace = true` **except the four dual-homed FFI crates**
(`qhyccd-rs` 1.85.0, `libqhyccd-sys` 1.68.0, `libzwo-sys` 1.70.0, `zwo-rs` 1.87.0), which
declare explicit lower MSRVs because they publish to crates.io for outside
consumers. Those lower floors cannot be verified in-workspace (the root
`profile.dev` needs Rust ≥ 1.71 and the shared lockfile pins newest deps), so the
in-workspace **msrv** job (`check.yml`) **skips** those four (the wrapper plus its
`sys-crate`, discovered from `[package.metadata.publish-readiness]`) and verifies
only the workspace-MSRV members. The four are instead checked out-of-tree by the
nightly **publish-readiness** workflow — see below and
[docs/plans/archive/publish-readiness-checks.md](../plans/archive/publish-readiness-checks.md).

### test.yml

`test.yml` runs on a nightly schedule (+ push to main + `workflow_dispatch`).
Bazel (`bazel.yml` + `bazel-coverage.yml`) is the per-PR
build/test/coverage gate, so this is a full-workspace Cargo safety net — every job
runs `--workspace` (no narrowing job).

| CI Job | Local Command | Prerequisites | Runs |
|--------|---------------|---------------|------|
| **required (stable)** | `cargo nextest run --locked --workspace --all-features --all-targets` + `cargo test --locked --workspace --all-features --test bdd` | stable, cargo-nextest | Off-PR |
| **required (stable, doc)** | `cargo test --locked --workspace --all-features --doc` | stable | Off-PR |
| **macos / windows** | same, per host OS (Windows runs BDD in one job) | -- | Off-PR |

The `macos` job runs with `RUSTFLAGS=-Dwarnings`. Nothing on a PR denies rustc
warnings for macOS — `bazel / macos-latest` is off the PR gate and clippy runs
on ubuntu only — so this job is where a macOS-only warning is enforced rather
than merely printed.

This workflow does not collect coverage — `bazel coverage` (bazel-coverage.yml)
is the sole coverage source.

**Doctests are the one test kind Bazel does not pick up on its own.** rules_rust
runs none unless a `rust_doc_test` target declares them, so only the crates that
have one — `rusty-photon-service-lifecycle` and `qhyccd-rs` (real + sim) — are in
the per-PR gate. Every other crate's examples are proven here, off-PR: a broken
example lands on main and surfaces in the next nightly. When you add examples to
a crate, add a `rust_doc_test` for it; if the crate has feature variants, give
each variant its own target and repeat `crate_features` on it, because
`rust_doc_test` does not inherit them from the crate it wraps.

**A RUNNABLE doctest on Windows hits `MAX_PATH` without the vendored rustdoc
patch.** `no_run` examples are emitted as metadata and never linked (rustdoc
reports them `- compile ... ok`), but one that actually runs invokes `link.exe`.
Before the fix that died on `LNK1181: cannot open input file
…libpanic_unwind-….rlib`: the file was present and readable, but its path was
261 characters — one over the limit — because `rust_doc_test` spells sysroot
inputs relative to the runfiles tree, whose prefix alone
(`…_doc_test.rustdoc_test.bat.runfiles\_main`) eats 123. The tell was a
pass/fail split between files in the *same directory*: `libstd-….rlib` at 252
resolved, `libpanic_unwind-….rlib` at 261 did not.

`third_party/patches/rustdoc_test_windows_external_repo_path.patch` now resolves
`--sysroot=` through the runfiles manifest to its execroot target, dropping that
path to 193. If you add a doctest target and see `LNK1181` again, the budget has
run out somewhere else — measure the path before theorising.

**A doctest target with `crate_features` needs the same patch on Windows.**
rustdoc receives the feature as `--cfg` plus `feature="simulation"`, and those
embedded quotes cannot ride a batch line: `cmd.exe` tracks quote state without
understanding any escape (`\"` still toggles it), so an inner quote ends a
`powershell.exe -c "…"` string early — with position-dependent symptoms, seen
both as `invalid --cfg argument` and as a silent exit-1 with a completely empty
test log. The vendored patch therefore writes the Windows runner's command into
a companion `.ps1` invoked via `powershell -File`, where single-quoted arguments
carry `"` literally and cmd.exe never parses them. One layer survives even
that: PowerShell 5.1 marshals arguments to a native child with embedded `"`
unescaped, so the child's CRT parser strips them — the runner's `CRT()`
function re-encodes them as `\"` per MSVCRT rules just before that hop. Both
together are what let the qhyccd-rs sim doctest target run on Windows.

**The runner also resolves its dependency paths through the runfiles
manifest.** windows-latest intermittently leaves a runfiles-tree entry as a
*dangling symlink* (the #587 family-2 flake: `file=false symlink=true len=0`
with a real parent directory), which no open-retry can outlast. The manifest's
execroot target is a real file — `build:windows --remote_download_outputs=all`
outranks `build:ci`'s `toplevel` because platform config appends last — so
RF() substitutes it and sidesteps the tree. That covers the `--arg-file`
values in both spellings: `external/…/_bs.linksearchpaths` for a crates.io
build script (#752), and bare workspace-relative
`crates/…/build_script.linksearchpaths` for a first-party path dependency's
(#781 — the qhyccd-rs real/sim doc-test pair shares that runfile and raced on
it whenever both executed on the same runner, exactly one failing per
attempt). It also covers the rlibs rustdoc itself opens, which arrive inside a
flag rather than bare: `--extern=<name>=<rlib>` per direct dependency, and the
documented crate's own rlib as the `<name>=<path>` value of a bare `--extern`
(#796).

`-L…=<dir>` search paths are deliberately left alone. A directory has no
manifest entry of its own, and inferring one from a file beneath it is
ambiguous whenever a repo contributes both sources and generated outputs to
the runfiles — `<output_base>/external/<repo>` versus
`<output_base>/execroot/…/bazel-out/<cfg>/bin/external/<repo>`. Rewriting them
resolved to the source directory, which contains no rlib. If you ever need to
resolve a directory here, prove which tree it lands in first.

**To see what the runner actually handed rustdoc**, run the doc tests with the
trace on:

```bash
bazel test --test_env=RUSTDOC_TEST_TRACE_ARGV=1 //crates/qhyccd-rs:all
```

Each resolved argv element is written to stderr as `RUSTDOC-ARGV <value>`,
which Bazel prints only for a failing test. That trace is what identified the
`-L` bug above; nothing else shows it, because resolution happens at test time
inside the generated script and rustdoc reports an argument it could not use
exactly like one it never received. It is off by default on purpose:
`--test_env` is part of every test's action key, so enabling it in `.bazelrc`
re-executes the entire suite on every platform, and that load surfaces
unrelated timing flakes.

**Reading a doc-test dependency failure on Windows.** rustdoc reports a lost
rlib two different ways, and which one you get says *where* the loss happened,
not what was lost:

| Message | What it proves |
|---------|----------------|
| `error: extern location for X does not exist: <path>` | the first of rustc's two stats of that path failed |
| `error[E0463]: can't find crate for X`, with **no** `note:` lines | the first stat succeeded and the second (inside the metadata loader) did not — `MetadataError::NotPresent` is swallowed without recording a rejection, so nothing names the file |
| `error[E0432]: unresolved import` | genuinely no `--extern` for that crate — an argv problem, not a filesystem one |
| `error[E0786]: found invalid metadata files` | the file is there but truncated or corrupt |

The first two mean the same defect; only the second is easy to misread as a
missing dependency edge. Both appeared for the same crate across two
occurrences on 2026-07-31. When one recurs, the job's "Dump doc-test runfiles
state (Windows diagnostic)" step reports, per doc test, the `_main\external`
link and every manifest-named `.rlib` that is unreachable by either spelling —
read that before theorising.

Two things generalise. The job's "Enable long paths" step does **not** cover
this: `LongPathsEnabled` is opt-in per binary via a `longPathAware` manifest, and
`link.exe` carries none (the step's own comment scopes it to `cl.exe`, which
does). And on Windows a "cannot open" whose path is near 260 characters is a
length problem — **measure it**; the file existing proves nothing.

### safety.yml

Nightly + push-to-main + `workflow_dispatch` (never on PRs). Both sanitizers run at
the workspace level.

| CI Job | Local Command | Prerequisites | Runs |
|--------|---------------|---------------|------|
| **address sanitizer** | See below | nightly, llvm | Off-PR |
| **leak sanitizer** | See below | nightly | Off-PR |

Both jobs export `ZWO_SKIP_NATIVE_LINK=1 QHYCCD_SKIP_NATIVE_LINK=1
SVBONY_SKIP_NATIVE_LINK=1` so the three native-SDK camera stacks build their
pure-Rust simulation path and need no SDK provisioned, exclude `bdd-infra`
(its unit tests look for binaries under `target/debug/`, which does not exist
once `--target` is set), and pass `--no-fail-fast` so one crate's report does
not hide the rest of the workspace.

Address sanitizer:

```bash
ASAN_OPTIONS="detect_odr_violation=0:detect_leaks=0" \
RUSTFLAGS="-Z sanitizer=address" \
ZWO_SKIP_NATIVE_LINK=1 QHYCCD_SKIP_NATIVE_LINK=1 SVBONY_SKIP_NATIVE_LINK=1 \
  cargo +nightly test --workspace --exclude bdd-infra --lib --tests --all-features \
    --no-fail-fast --target x86_64-unknown-linux-gnu
```

Leak sanitizer:

```bash
RUSTFLAGS="-Z sanitizer=leak" \
ZWO_SKIP_NATIVE_LINK=1 QHYCCD_SKIP_NATIVE_LINK=1 SVBONY_SKIP_NATIVE_LINK=1 \
  cargo +nightly test --workspace --exclude bdd-infra --all-features --all-targets \
    --no-fail-fast --target x86_64-unknown-linux-gnu
```

> **Note:** The sanitizers modify `Cargo.toml` in CI to set `[profile.dev] opt-level = 1`.
> Locally you can either do the same (and revert), or pass `-C opt-level=1` in
> `RUSTFLAGS` alongside the sanitizer flag. LeakSanitizer is documented as
> unreliable at `opt-level = 0`, so prefer one of the two over accepting the
> default.

LeakSanitizer counts an intentionally leaked allocation as a leak — `Box::leak`
to satisfy a `&'static` bound in a test is a real finding, not a false positive.
Build such fixtures as `static` items instead.

### conformu.yml (rolling)

ConformU runs on a nightly cron (05:30 UTC) and `workflow_dispatch` --
**not on PRs or push**. Conformance regressions are real but rare, and
the matrix is the most expensive workflow we have, so paying for it on
every PR is overkill. The faster `check`/`test` workflows already gate
the unit-level changes that would most often break conformance; the
nightly catches drift, and `workflow_dispatch` covers the "I just
touched the Alpaca interface, run it now" case. A `notify-on-failure`
job opens or updates a `conformu-nightly` labeled tracking issue when
a scheduled run fails.

| CI Job | Local Command | Prerequisites | Required? |
|--------|---------------|---------------|-----------|
| **plan** | `cargo metadata` + jq (see below) | jq | -- |
| **conformu** | Per-service command (see below) | ConformU | Optional |

The `plan` job exercises every conformu-tagged service (nightly + on-demand).
ConformU services are
discovered dynamically via `[package.metadata.conformu]` in each service's
`Cargo.toml`. To list them:

```bash
cargo metadata --format-version 1 --no-deps | \
  jq -r '.packages[] | select(.metadata.conformu.command) | "\(.name): \(.metadata.conformu.command)"'
```

Current services and their commands:
- **filemonitor**: `cargo test -p filemonitor --features conformu --test conformu_integration -- --ignored --nocapture`
- **ppba-driver**: `cargo test -p ppba-driver --features conformu --test conformu_integration -- --ignored --nocapture`
- **qhy-focuser**: `cargo test -p qhy-focuser --features conformu --test conformu_integration -- --ignored --nocapture`
- **sky-survey-camera**: `cargo test -p sky-survey-camera --features conformu --test conformu_integration -- --ignored --nocapture`

### pi-nightly.yml (rolling, self-hosted ARM64)

Runs the workspace build + tests on a Raspberry Pi 5 self-hosted runner
(Linux/ARM64) once per night. The only CI surface that exercises ARM64;
catches arch-specific regressions (atomics, alignment, vendored C deps
like `cfitsio`, cross-arch feature unification) that the x86 GitHub-hosted
runners cannot. **Triggers are deliberately limited to `schedule` and
`workflow_dispatch`** — never `pull_request` or `push` — because the repo
is public and a self-hosted runner accepting PR triggers would let forks
execute arbitrary code on the Pi. See
[docs/skills/raspberry-pi-runner.md](raspberry-pi-runner.md) for the full
security model, setup steps, and decommissioning procedure.

| CI Job | Local Command | Prerequisites | Required? |
|--------|---------------|---------------|-----------|
| **arm64-stable** | Same as scheduled `nightly` but on ARM64 stable: `cargo build --locked --workspace --all-features --all-targets` + `cargo nextest run --locked --all-features --all-targets` + `cargo test --locked --all-features --test bdd` + `cargo test --locked --all-features --doc` | self-hosted ARM64 runner, stable, cargo-nextest | Optional |
| **notify-on-failure** | N/A (CI-only — opens/updates a `pi-nightly` labelled issue when `arm64-stable` fails on a scheduled run) | -- | CI-only |

### publish-readiness.yml (rolling)

Pre-publish verification for the four **dual-homed FFI crates** (`qhyccd-rs` +
`libqhyccd-sys`, `zwo-rs` + `libzwo-sys`) — the published-in-isolation guarantees
the in-workspace `check`/`test` jobs cannot give. Nightly cron (02:30 UTC) +
`workflow_dispatch` + paths-filtered PR/push on the workflow and its script;
**non-blocking** for ordinary PRs (a minimal-versions break usually comes from an
upstream release, not the PR under review). Families are discovered dynamically via
`[package.metadata.publish-readiness]`. A green run is a **release prerequisite**
(see each crate's release runbook). Full design:
[docs/plans/archive/publish-readiness-checks.md](../plans/archive/publish-readiness-checks.md).

| CI Job | Local Command | Prerequisites | Required? |
|--------|---------------|---------------|-----------|
| **plan** | `cargo metadata` + jq (discovers FFI families) | jq | -- |
| **msrv-minimal-versions** | `scripts/verify-publishable-crate.sh <crate> verify` | nightly, cargo-hack, jq, rustup (auto-installs MSRV toolchains); libclang for zwo | Optional |
| **semver-checks** | `cargo semver-checks --package <crate>` | cargo-semver-checks | Optional |
| **docs** | `cargo +nightly docs-rs --package <crate>` | nightly, cargo-docs-rs | Optional |
| **find** (advisory) | `scripts/verify-publishable-crate.sh <crate> find` | cargo-msrv | CI-only (continue-on-error) |
| **notify-on-failure** | N/A (opens/updates a `publish-readiness` issue on scheduled red) | -- | CI-only |

The script copies each crate family OUT of the workspace and builds it on its
declared (lower) MSRV with a `-Z direct-minimal-versions` lockfile generated under
the MSRV-aware resolver (`CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback`) — the
two ingredients that let a low MSRV hold against minimal dependency versions. The
`*_SKIP_NATIVE_LINK` env makes it a check-only, SDK-free build (zwo still needs
libclang for bindgen, not the SDK binary).

### scheduled.yml (rolling)

These jobs only run on push to main, on schedule, or manually -- **not on PRs**.
No change detection is used; everything runs against the full workspace.

| CI Job | Local Command | Prerequisites | Required? |
|--------|---------------|---------------|-----------|
| **nightly** | `cargo +nightly nextest run --locked --all-features` + `cargo +nightly test --locked --all-features --test bdd` | nightly, cargo-nextest | Optional |
| **beta** | Same commands with `+beta` | beta toolchain | Optional |
| **discover-miri** | `cargo metadata` + jq | jq | -- |
| **miri** | Per-service command (see below) | nightly + miri component | Optional |
| **update** | `cargo +beta update && cargo +beta nextest run --locked --all-features` + `cargo +beta test --locked --all-features --test bdd` | beta, cargo-nextest | Optional |

Miri services are discovered dynamically via `[package.metadata.miri]` in each
service's `Cargo.toml`. To list them:

```bash
cargo metadata --format-version 1 --no-deps | \
  jq -r '.packages[] | select(.metadata.miri.command) | "\(.name): \(.metadata.miri.command)"'
```

Current services and their commands:
- **filemonitor**: `cargo miri test -p filemonitor`
- **phd2-guider**: `cargo miri test -p phd2-guider`
- **ppba-driver**: `cargo miri test -p ppba-driver`
- **qhy-focuser**: `cargo miri test -p qhy-focuser`
- **rp-auth**: `cargo miri test -p rp-auth`

> **Note:** Miri only runs on push to main (not on PRs) and requires
> `MIRIFLAGS="-Zmiri-disable-isolation"`. A clean build (`cargo clean`) is
> recommended before running miri to avoid stale artifact issues.

---

### ConformU Quick Start

```bash
# Install ConformU (first time only)
./scripts/test-conformance.sh --install-conformu

# Run conformance tests
./scripts/test-conformance.sh

# Run with custom options
./scripts/test-conformance.sh --port 12345 --verbose --keep-reports
```

---

## Quick Reference

Pre-push checks (copy-paste) — these mirror the full required gate (`bazel / <os>`,
`bazel coverage`, `stable / fmt`, `stable / clippy`); `fmt`/`clippy` are the
Cargo-only lint jobs Bazel doesn't cover:

```bash
bazel build //... && bazel test //...                     # bazel / <os> (build + tests incl. BDD; OmniSim suites need OMNISIM_PATH/OMNISIM_DIR)
bazel coverage --config=coverage //...                    # bazel coverage (heavier; needs OmniSim)
cargo fmt --check                                         # stable / fmt
cargo clippy --all-targets --all-features -- -D warnings  # stable / clippy pass 1
cargo clippy --workspace --lib --bins -- -D warnings      # stable / clippy pass 2 (#988)
```

## Bazel

Bazel is the per-PR build / test / coverage gate (`.github/workflows/bazel.yml`,
`bazel-coverage.yml`). `parity.yml` (Bazel/Cargo target parity) and the
Cargo build/test jobs run nightly as a safety net; `Cargo.toml` / `Cargo.lock`
remain the single source of truth for dependency versions.

Pre-push commands (these ARE the gate — run them before pushing):

```bash
bazel build //...
bazel test //...           # filters out tagged `requires-cargo` and `bdd`
```

**Warnings are errors in CI, not locally.** `--config=ci` passes `-Dwarnings` to
rustc for first-party code, so the CI Bazel jobs fail on any warning rather than
printing it. That is what makes a warning firing on only one target OS a gate:
the *required* `cargo clippy -D warnings` runs on ubuntu only, and CI builds all
three platforms. (Clippy's own OS gap is covered off-PR by check.yml's
`windows / clippy` + `macos / clippy` legs — see the check.yml section above.)
A plain local `bazel build //...` does *not* deny — the pre-commit hook already
runs `cargo clippy -- -D warnings` (a superset of rustc's lints) for your host
platform, and a Linux host cannot build the macOS/Windows targets where the
CI-only gap actually lives. Third-party crates are exempt either way:
`crate_universe` gives every crates.io target `--cap-lints=allow`, a hard ceiling
`-Dwarnings` cannot lift.

To reproduce a CI warning failure locally, add the flags to a plain build rather
than `--config=ci` (which also disables your disk cache):

```bash
bazel build --@rules_rust//rust/settings:extra_rustc_flags=-Dwarnings \
            --@rules_rust//rust/settings:extra_exec_rustc_flags=-Dwarnings //...
```

If you added a crates.io dependency, refresh the Bazel index:

```bash
# 2nd (un-forced) `bazel mod tidy` resets the lock's recorded CARGO_BAZEL_REPIN
# fingerprint to null, so the committed lock doesn't churn on later plain `bazel` runs.
CARGO_BAZEL_REPIN=1 bazel mod tidy && bazel mod tidy
# Required third step: `bazel mod tidy` only fixes up extensions it reaches via an
# explicit `use_repo()` in MODULE.bazel, so an extension pulled in transitively with
# no `use_repo()` of its own can be left out of the lock on some hosts. A full
# `--lockfile_mode=update` build does real target-graph analysis and fills the gap;
# skipping it produces a lock that `--lockfile_mode=error` rejects on x86_64 CI while
# `bazel mod tidy` alone looks fine locally.
bazel build --nobuild --lockfile_mode=update //...
git add MODULE.bazel.lock
```

`repin-bazel.yml` is gated on `github.actor == 'dependabot[bot]'`, so a
human PR that adds a crates.io dependency gets **no** automatic repin. A
forgotten repin turns all three `bazel / <os>` checks red with a
stale-lock error rather than a compile error.

Reviewing the result: `git diff --stat` badly under-reports a
`MODULE.bazel.lock` repin. The `cr` hub repo's `BUILD.bazel` and
`defs.bzl` are stored as single JSON string lines, so a 2-line diff can
carry a several-hundred-kilobyte changed-line payload. Use
`git diff --word-diff` or a JSON-aware differ.

BDD cucumber tests build and run under Bazel and are **part of the
default test filter** (since PR #452): a plain `bazel test //...` runs
them, and Bazel's result cache re-executes only the suites your change
affects. The OmniSim-backed suites need `OMNISIM_PATH` or `OMNISIM_DIR`
set. Doctor's `@pebble` ACME scenarios additionally use `PEBBLE_PATH` +
`PEBBLE_CHALLTESTSRV_PATH` when set, and skip loudly when not
(`docs/skills/testing.md` §5.6 — CI always provisions them, so a local
skip is not a green light for ACME-path changes). To narrow a run:

```bash
bazel test //services/filemonitor:bdd    # a single service's suite
bazel test --test_tag_filters=bdd //...  # only the BDD suites
```

Coverage runs as a separate required workflow
(`.github/workflows/bazel-coverage.yml`) on every PR. Locally it needs the
pinned nightly toolchain, which `--config=coverage` selects (see `.bazelrc`):

```bash
bazel coverage --config=coverage //...
# Combined lcov: $(bazel info output_path)/_coverage/_coverage_report.dat
```

It runs on the pinned nightly toolchain with `coverage_nightly` set, so the
`#[cfg(test)] mod tests` blocks stay out of the numbers — as do the
feature-gated `mock` transport/client modules, which carry a module-level
`#![cfg_attr(coverage_nightly, coverage(off))]` because they never ship in a
production binary and counting them would inflate the coverage figure with
code that never runs at the telescope. It uploads under the canonical
`<pkg>` Codecov flags that drive the per-service badges, and is the **sole**
coverage source (the Cargo jobs do not collect coverage). It **includes the BDD suite**
(`--config=coverage` drops only the `requires-cargo` tag), so locally it needs
OmniSim installed and `OMNISIM_PATH`/`OMNISIM_DIR` set, the same as a plain
`bazel test //...` run. Whether the BDD-spawned service
binaries' coverage is collected is validated in CI.

Known limitations:
- A few tests in `bdd-infra`, `phd2-guider`, and `filemonitor:test_cli`
  shell out to `cargo` or assume `target/debug` paths; they are tagged
  `requires-cargo` and skipped under Bazel.
- Conformu integration tests and Miri continue to run only under Cargo.

---

## Conditional Compilation Notes

- The `mock` feature is used by ppba-driver and qhy-focuser for integration
  testing (including ConformU). It is not required for normal builds.
- Feature powerset checks (`cargo hack --feature-powerset`) test all
  combinations of feature flags to verify features are additive -- this is
  important for feature unification in workspaces.

---

## Agent-Specific Notes

**Claude Code** users can run the full quality-gate suite via the `/pre-push`
slash command:

```
/pre-push          # All checks except miri
/pre-push miri     # All checks including miri
```

This command delegates to `act` with task-based parallelism.

---

## Troubleshooting

### Docker permission issues
```bash
sudo usermod -aG docker $USER
# Then log out and back in
```

### act not found
```bash
# Reinstall act
curl -s https://raw.githubusercontent.com/nektos/act/master/install.sh | sudo bash
sudo mv ./bin/act /usr/local/bin/
```

### Workflow fails locally but passes on GitHub
- Check environment variables in `.env`
- Ensure Docker has enough resources
- Some GitHub-specific features may not work locally

### Configuration files for act
- `.actrc`: act configuration (Docker images, settings)
- `.env`: Environment variables for workflows

### Tips
1. **First run takes longer**: Docker images need to be downloaded
2. **Use specific jobs**: Running entire workflows can be slow
3. **Check formatting first**: `cargo fmt --check` is the fastest check
4. **Memory usage**: Some jobs (like miri) require significant memory

---

## References

- [AGENTS.md](../AGENTS.md) -- Rule 4 (bazel build/test, fmt, clippy before committing)
- [Testing skill](testing.md) -- Writing and organizing tests
- `.github/workflows/` -- Workflow YAML files
- [GitHub Actions act](https://github.com/nektos/act) -- Local CI runner

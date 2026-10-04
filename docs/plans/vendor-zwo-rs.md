# Vendor `zwo-rs` + `libzwo-sys` into the workspace

**Status:** In progress — Phase 0 (ADR-010), Phase 1 (Cargo vendor) and Phase 2
(Bazel two-variant) **done** and verified on Linux; Phase 3 docs **done**.
macOS/Windows CI confirmation is **done**: the `native.yml` nightly workflow builds
the **real** (no-sim) `zwo-rs`/`libzwo-sys`/`zwo-camera` variant green via **Cargo**
(the MSVC source-of-truth path) on Linux/macOS/Windows, and `bazel.yml`'s
Windows/macOS matrix exercises the **Bazel** real variant — together retiring the
Windows real-variant bindgen-width risk on both toolchains. The **only remaining
work is owner-run and forward-only**: the first
crates.io publish (`libzwo-sys` then `zwo-rs`, `0.1.0`) + standalone-repo archival
— see the Release runbook in ADR-010.
**Author:** drafted 2026-06-17 on `worktree-zwo-driver`
**Sibling plan:** [`docs/plans/vendor-qhyccd-rs.md`](vendor-qhyccd-rs.md) — same
intention for the QHY FFI crates. This plan is the "Phase 4 (future)" that plan
explicitly deferred because zwo-rs adds **bindgen/libclang** on top of the same
shape.
**Supersedes:** ADR-008 / [`docs/plans/zwo-driver.md`](zwo-driver.md)'s "Canonical
home: each crate is its own repo … **not** vendored into the monorepo" decision,
and the test-only `zwo-rs = { features = ["simulation"] }` dev-dep that the
zwo-camera Bazel build currently relies on.

## Motivation

`zwo-rs` (`0.1.0`) and its FFI sub-crate `libzwo-sys` (`0.1.0`) are authored by us
(`ivonnyssen/zwo-rs`) but consumed as an **external git dependency** — the root
`Cargo.toml` pins
`zwo-rs = { git = "https://github.com/ivonnyssen/zwo-rs", rev = "3c32e59…" }`.
The `zwo-camera` service links the native ZWO ASI/EFW SDK through them
(`libzwo-sys` declares `links = "zwo"`, runs **bindgen** over vendored MIT headers
in its `build.rs`, and emits `dylib=ASICamera2` + `dylib=EFWFilter` + libusb/udev
link directives). Three frictions follow from the external status:

1. **Git-rev pin churn.** Every binding or simulation fix means a commit to the
   standalone repo + a `rev = …` bump in the monorepo (and a `cargo update -p`).
   This session alone bumped the rev twice (`73770f7` → `3c32e59`) to land two
   simulation fixes ConformU needed. For code we own, that is a release dance.
2. **No real/sim parity under Bazel.** `crate_universe` resolves **one** feature
   set per external crate and ignores a Bazel target's `crate_features`, so we
   cannot build a real-SDK *and* a `simulation` `zwo-rs` from `@cr`. The current
   workaround is a test-only `zwo-rs = { features = ["simulation"] }` dev-dep that
   flips the **single** resolved variant to `simulation` — so the (never-deployed)
   Bazel **production** binary is also simulated, and the real-SDK FFI path is
   never compiled under Bazel. (It also conveniently `cfg`s out the real FFI whose
   bindgen `c_long`/`c_int` widths the Bazel Windows toolchain handles differently
   — see Risks.) See [`docs/plans/archive/bazel-migration.md`](archive/bazel-migration.md),
   "External-crate non-default features for tests".
3. **Velocity tax.** Any binding change is a round-trip through a second repo
   rather than an in-tree edit, even though the two repos are developed in
   lockstep with the driver.

Making both crates **first-party workspace members** removes all three: the
git-rev pin disappears, we own the `BUILD.bazel` so we can author a true
two-variant build (real prod + `simulation` tests — the same pattern
`crates/rp-plate-solver` uses for its `_with_mock` variant), and edits are
in-place.

This is the same enabler the qhy vendoring plan identified: *"if you vendor for
the patch/velocity reasons, the clean real/sim two-variant comes along for free."*

## Scope

**In-scope:**
- Move `zwo-rs` (`src/`, docs, licenses, **vendored SDK headers**) and
  `libzwo-sys` (`src/lib.rs`, `build.rs`, `wrapper.h`, `sdk/include/`, `Cargo.toml`)
  into the workspace as members.
- Replace the `zwo-rs = { git = …, rev = … }` workspace dep with a path dep.
- Hand-write `BUILD.bazel` for both: a `cargo_build_script` for the **bindgen +
  native link** step (first first-party build-script crate in the repo, *unless*
  qhy vendoring lands first) and the real + `_sim` `rust_library` variants for
  `zwo-rs`.
- Re-point zwo-camera's Bazel targets at the first-party variants and drop the
  Bazel-only `simulation` dev-dep (subject to the rand/rayon spike below).
- Keep production Cargo + CI behaviour identical (real SDK linked, SDK binary
  still provisioned by `install-zwo-sdk`; vendored headers feed bindgen in-tree).
- **Dual-home (decided):** keep `zwo-rs` / `libzwo-sys` publishable to crates.io
  from the vendored subdirs, and perform the **first** `0.1.0` publish as part of
  Phase 3 (they are unpublished today).

**Out-of-scope (note, don't do here):**
- Changing the ZWO SDK provisioning (the `install-zwo-sdk` composite action, the
  INDI-mirror blobs, `/usr/local/lib`, `LIBCLANG_PATH`/`ZWO_SDK_LIB_DIR`
  forwarding) — all unchanged; the SDK binary is still required at link time
  regardless of where the crate source lives.
- EAF focuser support — `libzwo-sys` already *generates* EAF bindings; the
  library is linked only when the focuser is implemented. Vendoring carries the
  bindings along but adds no new linkage.
- Removing the SDK link requirement / a pure-Rust path — not possible.

## Key facts (verified 2026-06-17)

- **Source:** `/home/innyssen/repos/zwo-rs` @ `3c32e59` — **exactly the pinned
  rev**, so the vendor copy is a 1:1 snapshot of what the driver builds today.
  Layout: top-level `zwo-rs` crate + nested `libzwo-sys/` member.
- **`libzwo-sys` uses bindgen (the differentiator from qhy).** Its `build.rs`:
  - runs `bindgen` over `wrapper.h` (which `#include`s `ASICamera2.h`,
    `EFW_filter.h`, `EAF_focuser.h`), with `clang_args(["-x", "c++", "-std=c++14"])`
    and `-I sdk/include`, allowlisting `(ASI|EFW|EAF).*` → `OUT_DIR/bindings.rs`;
  - emits per-OS link directives: Linux → `/usr/local/lib` +
    `dylib=ASICamera2,EFWFilter,stdc++,usb-1.0,udev`; macOS → adds Homebrew lib
    dir, `c++`, `usb-1.0`, frameworks `IOKit`/`CoreFoundation`; Windows →
    `ASICamera2`/`EFWFilter` (lib dir via `ZWO_SDK_LIB_DIR`);
  - honours a `ZWO_SDK_LIB_DIR` override and declares `links = "zwo"`.
  - **bindgen needs libclang** (Linux: default; macOS: `LIBCLANG_PATH` to Homebrew
    llvm; both already provisioned by `install-zwo-sdk` and forwarded in
    `.bazelrc`). This is the one capability qhy's hand-written FFI does **not**
    need.
- **The MIT SDK headers are vendored in-tree** at `libzwo-sys/sdk/include/`
  (`ASICamera2.h`, `EFW_filter.h`, `EAF_focuser.h`, `license.txt`). They **must**
  travel into the monorepo — bindgen reads them at build time. (The SDK *binary*
  is **not** vendored; `install-zwo-sdk` still installs it.)
- **It already builds under Bazel today** via `crate_universe`'s auto-generated
  build-script target — i.e. **bindgen already runs inside the Bazel sandbox** on
  Linux/macOS/Windows in the shadow jobs, with libclang found and headers read.
  So Phase 2 re-expresses a **proven** build; the "first build-script crate +
  bindgen-in-Bazel" concern is materially de-risked. Inspect
  `@cr__libzwo-sys-0.1.0//` as the reference for the hand-written script.
- **`zwo-rs`** deps: `libzwo-sys` (path) + `thiserror`, `tracing`;
  `simulation = ["rand", "rayon"]` (both **optional**). Dev-deps: `mockall`,
  `cargo-husky` (precommit/user-hooks — **drop on vendor**, see Phase 1).
- **No first-party `cargo_build_script` exists yet** — this (or qhy) would be the
  first. `aws-lc-sys` (external) is the only build-script reference, handled by a
  `crate.annotation`.
- **Two-variant precedent:** `crates/rp-plate-solver` ships `rp-plate-solver`
  (prod) + `rp-plate-solver_with_mock` (`testonly`, `crate_features=["mock"]`,
  same `crate_name`). This is exactly the shape `zwo-rs` / `zwo-rs_sim` takes.
- **No `[patch.crates-io]` block exists** (zwo is a git-rev dep, not a
  patched crates.io dep) — so unlike qhy there is no patch to delete, only a
  git-rev dep to convert to a path dep.

## Target layout

```
crates/
├── zwo-rs/
│   ├── Cargo.toml          ← libzwo-sys dep stays { version = "0.1.0", path = "libzwo-sys" }
│   ├── src/  README.md  CHANGELOG.md  LICENSE-MIT  LICENSE-APACHE
│   ├── BUILD.bazel         ← zwo-rs (real) + zwo-rs_sim (testonly, simulation)
│   └── libzwo-sys/
│       ├── Cargo.toml      ← links = "zwo"
│       ├── src/lib.rs  build.rs  wrapper.h
│       ├── sdk/include/    ← vendored MIT headers (ASICamera2.h, EFW_filter.h, EAF_focuser.h, license.txt)
│       └── BUILD.bazel     ← cargo_build_script (bindgen + link) + rust_library
```

Nesting `libzwo-sys` under `crates/zwo-rs/` preserves the upstream path dep
(`libzwo-sys = { version = "0.1.0", path = "libzwo-sys" }`) verbatim and eases any
future upstream sync. Both are added to `[workspace] members`. Names stay
**unprefixed** (external-SDK bindings — per the crate-naming convention), matching
the qhy decision.

## Phases

### Phase 0 — Decisions (settled)
- **Layout: nested** (mirrors qhy ADR-009).
- **Publishing: dual-home** — keep both crates publishable to crates.io from the
  vendored subdirs; the monorepo is the canonical development home and single
  source of truth. Because they are **unpublished today**, this includes the
  **first** `0.1.0` publish (Phase 3) to prove the monorepo release path, after
  which the standalone repo is archived. (User decision, 2026-06-17.)
- **Record: ADR-010** (`docs/decisions/010-vendor-zwo-rs.md`) — citing ADR-001
  Amendment A (cfitsio purge) as the native-link precedent, ADR-008 (the
  zwo-camera native-SDK decision this amends), and ADR-009 (the qhy sibling). If
  branch merge order shuffles ADR numbers, renumber.

**Exit:** ADR-010 stub written; this section reflects the settled choices.

### Phase 1 — Vendor on the Cargo side (no Bazel yet)
- Copy `zwo-rs` + `libzwo-sys` source into `crates/zwo-rs/…`, **including
  `libzwo-sys/sdk/include/` and `wrapper.h`** (bindgen inputs). Drop the source
  repo's `target/`, standalone `Cargo.lock`, `.git`, `.github/`, `ci/`,
  `.cargo-husky/`, and `AGENTS.md`/`CLAUDE.md` symlink — repo-local tooling that
  the monorepo replaces.
- **Drop the `cargo-husky` dev-dependency** from `crates/zwo-rs/Cargo.toml`: its
  `user-hooks`/`precommit-hook` features install git hooks into the consuming
  repo, which would fight the monorepo's own pre-push gate. Keep `mockall`.
- Add both crates to `[workspace] members`.
- **Preserve publishability (dual-home).** Keep each vendored manifest's explicit
  `version` / `edition` / `rust-version` / `license` / `authors` / `description` /
  `repository` / `keywords` / `categories` — do **NOT** switch them to
  `*.workspace = true`; they release on their own cadence (`zwo-rs` 0.1.0,
  `libzwo-sys` 0.1.0). Keep `CHANGELOG.md` / `README.md` / `LICENSE-MIT` /
  `LICENSE-APACHE` in each crate, and `sdk/include/license.txt` (the SDK's MIT
  notice must travel with the headers). Keep the
  `libzwo-sys = { version = "0.1.0", path = "libzwo-sys" }` dual dep verbatim —
  `path` drives dev/Bazel builds, `version` drives `cargo publish`.
- Change the workspace dep `zwo-rs = { git = …, rev = … }` →
  `zwo-rs = { path = "crates/zwo-rs" }`.
- zwo-camera `Cargo.toml`: the normal `zwo-rs = { workspace = true }` is
  unchanged; **leave the `simulation` dev-dep in place for now** (Phase 2 decides
  its fate — see rand/rayon spike).
- `cargo update` / regenerate `Cargo.lock`; confirm the resolved `zwo-rs` +
  `libzwo-sys` are now the path crates and `rand`/`rayon` remain locked.
- **Verify Cargo parity** (SDK must be on the link path — `/usr/local/lib`
  ldconfig'd, libclang present):
  - `cargo build -p zwo-camera` → links the **real** SDK (prod path unchanged).
  - `cargo rail run --profile commit -q` → zwo-camera's unit + BDD suites green
    (`--all-features` so the `simulation` paths compile).
  - `cargo test -p zwo-rs --features simulation` (the vendored crate's own suite)
    green — now running as a workspace member.

**Exit:** git-rev pin gone; workspace builds/tests via path deps; no behaviour
change.

### Phase 2 — Bazel: build script (bindgen + link) + two variants
- `crates/zwo-rs/libzwo-sys/BUILD.bazel`:
  - `cargo_build_script(name = "build_script", srcs = ["build.rs"],
    data = glob(["sdk/include/**", "wrapper.h"]), …)` — the script both **runs
    bindgen** (needs libclang in the sandbox + the headers as `data`/`srcs`) and
    emits the link directives. `rules_rust` auto-provides
    `CARGO_CFG_TARGET_OS`/`_ARCH`; forward `LIBCLANG_PATH` (macOS) and
    `ZWO_SDK_LIB_DIR` (Windows) — already in `.bazelrc`. Mirror what
    `crate_universe` generates for `@cr__libzwo-sys-0.1.0//` today.
  - `rust_library(name = "libzwo-sys", crate_name = "libzwo_sys",
    deps = [":build_script"])`.
- `crates/zwo-rs/BUILD.bazel`:
  - `rust_library(name = "zwo-rs", crate_features = [],
    deps = [":libzwo-sys", "@cr//:thiserror", "@cr//:tracing"])` — **real** SDK.
  - `rust_library(name = "zwo-rs_sim", testonly = True, crate_name = "zwo_rs",
    crate_features = ["simulation"], deps = [… , "@cr//:rand", "@cr//:rayon"])` —
    simulated. `testonly = True` makes Bazel reject any production target that
    links it.
- zwo-camera `BUILD.bazel`:
  - Replace the `zwo-rs` entry from `all_crate_deps(normal=True)` with an explicit
    dep (it's no longer an `@cr` crate). Prod targets (`zwo-camera_lib`,
    `zwo-camera`, `zwo-camera_unit_test`*) → `//crates/zwo-rs:zwo-rs`; sim targets
    (`zwo-camera_lib_sim`, `zwo-camera_sim`, `bdd`, `conformu_integration`) →
    `//crates/zwo-rs:zwo-rs_sim`.
  - *Note `zwo-camera_unit_test` currently sets `crate_features = ["simulation"]`
    to compile the `#[cfg(all(test, feature = "simulation"))]` handle tests — it
    must therefore depend on `zwo-rs_sim`, not the real variant. Keep that
    feature on the zwo-camera *own* crate; the underlying zwo-rs comes from the
    sim variant. (The real-variant unit coverage is the prod lib/binary
    compiling.)
  - Keep `crate_features = ["mock"/"simulation"]` on the zwo-camera sim targets
    (that drives zwo-camera's *own* cfg; still required).
- **rand/rayon availability (the one real nuance — spike first).** `@cr//:rand`
  and `@cr//:rayon` exist only if Cargo resolution reaches `zwo-rs/simulation`.
  With `zwo-rs` now first-party, run `bazel query @cr//:rayon` after a
  **no-dev-dep** repin. If **present**, drop the zwo-camera `simulation` dev-dep
  entirely. If **absent**, the dev-dep stays but its role shrinks to "keep
  simulation's optional deps in `@cr`" (document that), or pull them via a
  `crate.annotation`.
- `scripts/repin-bazel-lock.sh` (Rule 10), then
  `git diff --cached MODULE.bazel.lock`.
- **Verify Bazel** (shadow jobs run `install-zwo-sdk` first):
  - `bazel build //crates/zwo-rs/...` + `//services/zwo-camera/...`.
  - Confirm the **prod** `zwo-camera` binary now compiles + links the **real**
    SDK FFI (the parity win) — and that this holds on the Bazel Windows toolchain
    (see Risks: bindgen integer widths).
  - `bazel test //services/zwo-camera:zwo-camera_unit_test`,
    `:bdd --test_tag_filters=bdd,-requires-cargo`,
    `:conformu_integration --config=conformu` → all green, sim binaries still
    simulated (0 USB).
  - `bazel build --nobuild //... --lockfile_mode=error` → no graph drift.

**Exit:** real prod / sim test under Bazel; dev-dep dropped or its role
documented.

### Phase 3 — Publish, cleanup + docs
- **First crates.io publish (dual-home).** `cargo publish` `libzwo-sys` `0.1.0`
  first, then `zwo-rs` `0.1.0`, from the vendored subdirs (`cargo publish`
  rewrites the path dep to the `version`). This proves the monorepo release path
  before the standalone repo is retired. Add a **release runbook** (publish order;
  `--allow-dirty` not needed; verify on docs.rs).
- Update `docs/services/zwo-camera.md` "Native dependency & build gating": replace
  the git-rev + dev-dep narrative with the first-party two-variant story; note the
  pin is gone and bindgen now runs in-tree.
- Update `docs/plans/archive/bazel-migration.md` "External-crate non-default features"
  subsection: note zwo-camera moved to the first-party two-variant pattern (and
  that the dev-dep technique remains the answer for crates we *don't* vendor).
- Update ADR-008 status (or add an amendment) pointing at ADR-010 for the
  canonical-home reversal; update [`docs/plans/zwo-driver.md`](zwo-driver.md)'s
  "Canonical home" / "Dev model" rows.
- Update the memory notes ([[zwo-camera-phase-e]], [[zwo-camera-phase-c]],
  MEMORY.md Build Notes): zwo-rs is now first-party, dual-homed; git-rev pin gone;
  real/sim split.
- `cargo fmt`; buildifier the new BUILD files if available (match surrounding
  style by hand if not on PATH).
- **Hard-archive the standalone `ivonnyssen/zwo-rs` repo** — but only *after* the
  first `cargo publish` from the vendored subdirs confirms the monorepo release
  path works. Point its README at the monorepo, then GitHub → Settings → Archive
  (read-only).

**Exit:** crates published from the monorepo; docs/memory consistent; `cargo rail`
+ Bazel shadow jobs green.

## Risks & mitigations

| Risk | Likelihood | Mitigation |
|---|---|---|
| **bindgen in the Bazel sandbox** (libclang discovery, header `data`, C++14 parse) misbehaves in a hand-written `cargo_build_script` | Low–Med | `crate_universe` already runs this exact `build.rs` (bindgen included) under Bazel on all three OSes today; copy its generated target as the reference. `LIBCLANG_PATH`/`ZWO_SDK_LIB_DIR` already forwarded in `.bazelrc`. |
| **Windows real-variant bindgen integer widths** — building the *real* `zwo-rs` under Bazel's Windows toolchain may surface the `c_long`/`c_int` width difference that forcing `simulation` currently `cfg`s out | ~~Medium~~ **Resolved** | Both toolchains now build the real variant green on Windows: `native.yml` (Cargo MSVC, the source of truth) and `bazel.yml`'s Windows matrix (Bazel). The width difference did not materialise; no `cfg` workaround was needed. |
| `rand`/`rayon` not in `@cr` after dropping the dev-dep | Medium | Spike (`bazel query @cr//:rayon`) before dropping; keep the dev-dep (role-shifted) or add a `crate.annotation`. Wiring choice, not a blocker. |
| Vendored SDK headers / `license.txt` dropped on copy → bindgen can't find them | Low | Phase 1 explicitly copies `sdk/include/**` + `wrapper.h`; `cargo build -p zwo-rs` in Phase 1 fails fast if missing. |
| `cargo-husky` dev-dep installs git hooks into the monorepo | Low | Dropped in Phase 1. |
| First crates.io publish from a subdir is awkward / version already taken | Low | `cargo publish` works from a member dir; `zwo-rs`/`libzwo-sys` 0.1.0 names confirmed available (zwo-driver plan). Publish behind a runbook. |
| Workspace feature-unification surprises (zwo-rs deps now unified with the whole workspace) | Low | Phase 1 verifies `cargo rail` + the vendored crate's own tests before any Bazel work. |

## Rollback
Each phase is independently revertible. Phase 1 rollback = restore the
`zwo-rs = { git = …, rev = … }` workspace dep and delete the vendored dirs. Phase 2
rollback = restore the `simulation` dev-dep + the `all_crate_deps` zwo-rs entry and
repin. Phase 3's publish is forward-only (a published 0.1.0 can't be unpublished),
so treat the publish as the point of no return — everything before it is
reversible. Nothing here changes production release packaging (Cargo, unchanged).

## Open questions
- **Drop the dev-dep entirely?** Resolved by the Phase 2 rand/rayon spike (keep it,
  role-shifted, if `@cr` loses rand/rayon without it).
- **Windows real-variant under Bazel** — **resolved**: prod targets build the real
  variant everywhere; `native.yml` (Cargo) and `bazel.yml`'s Windows matrix both
  link the real SDK green, so no non-Windows carve-out was needed.
- **EAF headers** travel in `sdk/include/` but `libEAFFocuser` is unlinked until
  focuser work — no action now; noted so the bindings aren't mistaken for dead
  code during the vendor copy.

# ADR-010: Vendor `zwo-rs` + `libzwo-sys` into the workspace (dual-homed)

## Status

Accepted (2026-06-17); **implemented** on `worktree-zwo-driver` / PR #369 —
tracked by [`docs/plans/vendor-zwo-rs.md`](../plans/vendor-zwo-rs.md). Phase 0
(decisions) is settled by this ADR; Phase 1 (Cargo vendor → path dep), Phase 2
(Bazel real/sim two-variant), and Phase 3 docs are landed and verified on Linux.
**Pending:** the first crates.io publish (`libzwo-sys` then `zwo-rs` 0.1.0) and the
standalone-repo archival — both forward-only, owner-run (see the Release runbook
below) — plus macOS/Windows CI confirmation.

Amends [ADR-008](008-zwo-camera-native-sdk-ffi.md)'s "canonical home" posture (the
FFI crates as standalone, separately-published repos, consumed by the driver as a
git-rev dependency). The sibling of [ADR-009](009-vendor-qhyccd-rs.md) (vendoring
the QHY FFI crates), and the "Phase 4" that
[`docs/plans/vendor-qhyccd-rs.md`](../plans/vendor-qhyccd-rs.md) deferred because
`zwo-rs` adds bindgen/libclang.

Supersedes the interim Bazel fix on `worktree-zwo-driver` (a test-only
`zwo-rs = { features = ["simulation"] }` dev-dep + `simulation` `crate_features`;
see [`docs/plans/archive/bazel-migration.md`](../plans/archive/bazel-migration.md), "External-crate
non-default features for tests").

## Context

`zwo-rs` (`0.1.0`) and its FFI sub-crate `libzwo-sys` (`0.1.0`) are authored by us
(`ivonnyssen/zwo-rs`). *Before this change* they were consumed as an **external
git dependency** — the root `Cargo.toml` pinned
`zwo-rs = { git = "https://github.com/ivonnyssen/zwo-rs", rev = "3c32e59…" }` — and
were **unpublished** (no crates.io release). This ADR replaces that pin with a path
dep to the vendored `crates/zwo-rs`. The `zwo-camera` service links the
native ZWO ASI/EFW SDK through them: `libzwo-sys` declares `links = "zwo"`, runs
**bindgen** over vendored MIT headers (`sdk/include/{ASICamera2,EFW_filter,
EAF_focuser}.h`, parsed as C++ for the bare `bool`) in its `build.rs`, and emits
`dylib=ASICamera2` + `dylib=EFWFilter` + libusb / libudev / libc++ link
directives. Three frictions follow from the external status:

1. **Git-rev pin churn.** Every binding or simulation fix is a commit to the
   standalone repo + a `rev = …` bump + `cargo update -p`. The current `3c32e59`
   pin is the second bump in a single session (two ConformU-driven simulation
   fixes).
2. **No real/sim parity under Bazel.** `crate_universe` resolves **one** feature
   set per external crate and ignores a Bazel target's `crate_features`, so we
   cannot build a real-SDK *and* a `simulation` `zwo-rs` from `@cr`. The interim
   workaround flips the single resolved variant to `simulation` via a dev-dep — so
   the (never-deployed) Bazel **production** binary is also simulated, and the real
   FFI path is never compiled under Bazel. (It also `cfg`s out the real FFI whose
   bindgen `c_long`/`c_int` widths the Bazel Windows toolchain treats differently.)
3. **Velocity tax.** Any binding change round-trips through a second repo rather
   than being an in-tree edit, despite the two repos being developed in lockstep
   with the driver.

This mirrors [ADR-001 Amendment A](001-fits-file-support.md) (the cfitsio purge) as
a native-link dependency question, and [ADR-009](009-vendor-qhyccd-rs.md) as the
QHY sibling. The decisive way `zwo-rs` differs from `qhyccd-rs`: it uses **bindgen**
(needs libclang + vendored headers) and it is consumed as a **git-rev** dep (no
`[patch.crates-io]` block) and is **unpublished**.

## Decision

Move `zwo-rs` and `libzwo-sys` into the workspace as **first-party, nested,
dual-homed** members.

### Nested layout

`crates/zwo-rs/` holds the crate, with `libzwo-sys/` nested inside it — preserving
the upstream `libzwo-sys = { version = "0.1.0", path = "libzwo-sys" }` dependency
verbatim, and carrying the **vendored MIT SDK headers** (`libzwo-sys/sdk/include/`)
+ `wrapper.h` that bindgen reads at build time. Both are added to
`[workspace] members`. (Flat `crates/zwo-rs` + `crates/libzwo-sys` was the
alternative; nested wins on minimal churn and on keeping the sys-crate visibly part
of zwo-rs.) Names stay unprefixed (external-SDK bindings — per the crate-naming
convention).

### Dual-home (publish to crates.io)

The monorepo becomes the **canonical development home** and the single source of
truth; we publish both crates to crates.io **from the vendored subdirs** for
outside consumers (a Rust ASI/EFW binding does not otherwise exist — see ADR-008).
Because they are **unpublished today**, this includes the **first** `0.1.0` publish
(`libzwo-sys` then `zwo-rs`), which also proves the monorepo release path before the
standalone repo is retired. This works cleanly because the inter-crate dep carries
both a `version` and a `path`: `cargo publish` rewrites the path dep to the `version`,
while local + Bazel builds use the `path`. To stay publishable, the vendored
manifests **keep independent explicit `version` / `edition` / `rust-version` /
`license` / `authors` / `description` / `keywords` / `categories`** (NOT
`*.workspace = true` — they release on their own cadence) and retain their
`CHANGELOG` / `README` / `LICENSE-MIT` / `LICENSE-APACHE` and the SDK headers'
`license.txt`. The standalone `ivonnyssen/zwo-rs` repo is **hard-archived**
(read-only, README pointing at the monorepo) once the first publish-from-monorepo is
verified.

### Two Bazel variants (the parity win)

With the crates first-party we own their `BUILD.bazel`, so we apply the existing
first-party variant pattern (`crates/rp-plate-solver` ↔ `rp-plate-solver_with_mock`):

- a `cargo_build_script` for `libzwo-sys` (the repo's **first** first-party
  build-script crate — and one that runs **bindgen** in the sandbox; modelled on
  what `crate_universe` generates for it today, where bindgen already runs on all
  three OSes, so it is a re-expression of a proven build, not a new one), plus its
  `rust_library`;
- `zwo-rs` (real SDK, no features) **and** `zwo-rs_sim` (`testonly = True`,
  `crate_features = ["simulation"]`, same `crate_name`).

`zwo-camera`'s production targets depend on the real `zwo-rs`; its sim / BDD /
ConformU targets depend on `zwo-rs_sim`. `testonly = True` makes Bazel reject any
production target that accidentally links the sim variant.

## Consequences

- **The git-rev pin is deleted.** SDK-binding and simulation fixes are in-tree
  edits; no rev bump, no `cargo update -p`.
- **Real/sim parity under Bazel:** the production `zwo-camera` Bazel binary links
  the **real** SDK (and compiles the real FFI path) while BDD / ConformU stay
  simulated. The interim `simulation` dev-dep is dropped — *unless* the Phase 2
  spike shows `rand`/`rayon` (simulation's optional deps) leave `@cr` without it,
  in which case it stays with a narrowed role (dep-availability only).
- **First first-party `cargo_build_script`** in the repo, and the first to run
  **bindgen** under Bazel from a hand-written script. De-risked: the same
  `build.rs` already runs under Bazel via `crate_universe` today; `LIBCLANG_PATH`
  (macOS) / `ZWO_SDK_LIB_DIR` (Windows) are already forwarded in `.bazelrc`. SDK
  provisioning (the `install-zwo-sdk` action, INDI-mirror blobs, `/usr/local/lib`)
  is unchanged.
- **A new Windows risk surfaces:** building the *real* `zwo-rs` variant under
  Bazel's Windows toolchain may hit the `c_long`/`c_int` bindgen width difference
  that forcing `simulation` currently sidesteps. The Phase 2 Windows spike resolves
  it (flip prod targets to real everywhere, or keep the prod-real target
  non-Windows). The Cargo prod build (MSVC) is the source of truth regardless.
- **A small convention deviation:** these two members keep explicit, independent
  package **identity metadata** (`version` / `edition` / `rust-version` /
  `license` / `authors` / `description` / …) rather than `*.workspace = true`,
  because they are independently published on their own cadence. Their **shared
  dependencies** still inherit the workspace pin (`dep.workspace = true`,
  Rule 10) — `cargo publish` flattens those to concrete versions, so the
  standalone release is unaffected (verified by dry-run). Documented here, in the
  plan, and in [`docs/workspace.md`](../workspace.md) "Workspace Dependencies".
- **The `cargo-husky` dev-dep is dropped** on vendoring (its git-hook installation
  would fight the monorepo's pre-push gate).
- **A release runbook** is added: bump + `cargo publish` `libzwo-sys` first, then
  `zwo-rs`. `MODULE.bazel` is unchanged (members auto-discovered); a
  `CARGO_BAZEL_REPIN` is still needed when the vendored crates' *external* deps
  change.
- **`zwo-rs` is the second user of the vendoring template** (after `qhyccd-rs`),
  and the first to exercise the bindgen-in-`cargo_build_script` path — making it
  the reference for any future bindgen-based `*-sys` crate.

## Release runbook (dual-home publish to crates.io)

The crates publish from their vendored subdirs; `cargo publish` rewrites the
inter-crate `path` dep to its `version`. Publish the sys-crate first so the
wrapper's dependency resolves on crates.io.

1. **Publish-readiness MUST be green** for the crate being released — it verifies
   the published-in-isolation guarantees (MSRV, direct-minimal-versions, semver,
   docs.rs) that the in-workspace checks cannot: `gh workflow run
   publish-readiness.yml` (or rely on the last green nightly). A red run BLOCKS the
   release. The crates declare explicit lower MSRVs (`libzwo-sys` 1.71.0, `zwo-rs`
   1.87.0 — the latter pending an `is_multiple_of` refactor to reach ~1.71); if a
   change raises a floor, bump that crate's `rust-version`. See
   [docs/plans/archive/publish-readiness-checks.md](../plans/archive/publish-readiness-checks.md).
2. Bump `version` in `crates/zwo-rs/libzwo-sys/Cargo.toml` and/or
   `crates/zwo-rs/Cargo.toml` as needed; update each crate's `CHANGELOG.md`.
3. If the wrapper's `version` changed, bump the `libzwo-sys = { version = "…" }`
   requirement in `crates/zwo-rs/Cargo.toml` to match.
4. `cargo publish -p libzwo-sys` (needs the ZWO SDK + libclang locally — the
   build script links + runs bindgen). Wait for it to land on crates.io.
5. `cargo publish -p zwo-rs`.
6. Verify on docs.rs / crates.io; tag the release.

`MODULE.bazel` is unchanged across a publish (members are auto-discovered). A
`CARGO_BAZEL_REPIN=1 bazel mod tidy && bazel mod tidy` is needed only when the
vendored crates' **external** deps change (not on a version bump alone). The
standalone `ivonnyssen/zwo-rs` repo is archived once the first publish-from-monorepo
is verified; thereafter the monorepo is the sole source of truth.

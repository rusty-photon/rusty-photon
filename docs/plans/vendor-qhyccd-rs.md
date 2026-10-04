# Vendor `qhyccd-rs` + `libqhyccd-sys` into the workspace

**Status:** Phases 1 & 2 DONE (2026-06-17, `worktree-qhy-camera`); Phase 3
**docs done** (qhy-camera.md, bazel-migration.md, memory, fmt). The only
remaining Phase 3 work is the **first `cargo publish` from the vendored
subdirs** and the subsequent **standalone-repo archive**, both deferred by
design until the publish proves the release path — see the runbook at
[`crates/qhyccd-rs/RELEASING.md`](../../crates/qhyccd-rs/RELEASING.md).
**Author:** drafted 2026-06-17 on `worktree-qhy-camera`
**Depends on:** the qhy-camera Bazel simulation fix (dev-dep + `crate_features`)
already landed on this branch — this plan supersedes the variant-flipping role of
that dev-dep (the dev-dep itself is retained for a narrower reason; see Phase 2).

## Motivation

`qhyccd-rs` (`=0.1.9`, crates.io) and its FFI sub-crate `libqhyccd-sys` (`0.1.4`)
are authored by us (`ivonnyssen/qhyccd-rs`) but consumed as **external** crates.
Three frictions follow from that:

1. **A `[patch.crates-io]` git override.** `libqhyccd-sys 0.1.4` on crates.io was
   cut before the macOS link fix, so the root `Cargo.toml` pins it to a git rev
   (`d84f867…`). Every SDK-binding fix means cutting an upstream commit and bumping
   the rev — a release dance for code we own.
2. **No real/sim parity under Bazel.** Because `crate_universe` resolves **one**
   feature set per external crate (and ignores a Bazel target's `crate_features`),
   we can't build a real-SDK *and* a `simulation` `qhyccd-rs` from `@cr`. The
   current workaround is a test-only `qhyccd-rs = { features = ["simulation"] }`
   dev-dep that flips the **single** resolved variant to simulation — which means
   the (never-deployed) Bazel production binary is also simulated. See
   docs/plans/archive/bazel-migration.md, "External-crate non-default features for tests".
3. **Repin churn.** Any binding change needs a crates.io publish (or rev bump) +
   `CARGO_BAZEL_REPIN`, rather than an in-tree edit.

Making both crates **first-party workspace members** removes all three: the patch
disappears, we own the `BUILD.bazel` so we can author a true two-variant build
(real prod + `simulation` tests, the same pattern `crates/rp-plate-solver` uses
for its `_with_mock` variant), and edits are in-place.

This is the enabler identified in the qhy-camera Bazel discussion: *"if you vendor
for the patch/velocity reasons, the clean real/sim two-variant comes along for
free."* The user has decided to pursue it.

## Scope

**In-scope:**
- Move `qhyccd-rs` (`src/`, `tests/`, docs, licenses) and `libqhyccd-sys`
  (`lib.rs`, `build.rs`, `Cargo.toml`) into the workspace as members.
- Delete the `[patch.crates-io]` block and the `qhyccd-rs = "=0.1.9"` crates.io
  workspace dep; replace with path deps.
- Hand-write `BUILD.bazel` for both: a `cargo_build_script` for the native
  `static=qhyccd` link (first first-party build-script crate in the repo) and the
  real + `_sim` `rust_library` variants for `qhyccd-rs`.
- Re-point qhy-camera's Bazel targets at the first-party variants and drop the
  Bazel-only `simulation` dev-dep (subject to the rand/rayon nuance below).
- Keep production Cargo + CI behaviour identical (real SDK linked, SDK still
  provisioned by the install action).

**Out-of-scope (note, don't do here):**
- Vendoring `zwo-rs`/`libzwo-sys` (the user owns that). It has the *same* shape
  but adds **bindgen** (libclang) on top, so it's a separate, larger effort. This
  plan's `cargo_build_script` pattern is the template for it later.
- Changing the QHYCCD SDK provisioning (install action, `/usr/local/lib`,
  `GITHUB_WORKSPACE` forwarding) — all unchanged; the static SDK is still required
  at link time regardless of where the crate source lives.
- Removing the SDK link requirement / a pure-Rust path — not possible.

## Key facts (verified 2026-06-17)

- **Source:** `/home/innyssen/repos/qhyccd-rs` @ `29641e2` (contains the macOS fix
  the `d84f867` patch points at). Layout: top-level `qhyccd-rs` crate + nested
  `libqhyccd-sys/` member.
- **`libqhyccd-sys` is simple:** `lib.rs` is a **hand-written** `extern "C"` block
  (no bindgen → **no libclang** needed, unlike zwo-rs). `Cargo.toml` declares
  `links = "qhyccd"`. `build.rs` emits per-OS directives: Linux →
  `rustc-link-search=/usr/local/lib` + `static=qhyccd` + `dylib=usb-1.0` +
  `dylib=stdc++`; macOS/Windows read `GITHUB_WORKSPACE` (already forwarded in
  `.bazelrc`) then fall back to system/Homebrew paths.
- **It already builds under Bazel today** via `crate_universe`'s auto-generated
  build-script target — so the link behaviour (`/usr/local/lib`, sandbox
  read-mount) is proven; Phase 2 just re-expresses it in a hand-written
  `cargo_build_script`. De-risks the "first build-script crate" concern.
- **`qhyccd-rs`** deps: `libqhyccd-sys` (path) + `eyre`, `thiserror`, `tracing`,
  `tracing-subscriber`, `educe`, `lazy_static`, `tracing-attributes`,
  `enum-ordinalize-derive`; `simulation = ["rand", "rayon"]` (both **optional**).
- **No first-party `cargo_build_script` exists yet** — `libqhyccd-sys` is the
  first. `aws-lc-sys` (external) is the only build-script reference, handled by a
  `crate.annotation`.
- **Two-variant precedent:** `crates/rp-plate-solver` ships `rp-plate-solver`
  (prod) + `rp-plate-solver_with_mock` (`testonly`, `crate_features=["mock"]`,
  same `crate_name`). This is exactly the shape `qhyccd-rs` / `qhyccd-rs_sim`
  takes.

## Target layout

```
crates/
├── qhyccd-rs/
│   ├── Cargo.toml          ← libqhyccd-sys dep becomes { path = "libqhyccd-sys" } (unchanged if nested)
│   ├── src/  tests/  docs/
│   ├── BUILD.bazel         ← qhyccd-rs (real) + qhyccd-rs_sim (testonly, simulation)
│   └── libqhyccd-sys/
│       ├── Cargo.toml      ← links = "qhyccd"
│       ├── lib.rs  build.rs
│       └── BUILD.bazel     ← cargo_build_script + rust_library
```

Nesting `libqhyccd-sys` under `crates/qhyccd-rs/` preserves the upstream path dep
(minimises churn to the vendored `Cargo.toml`s and eases any future upstream
sync). Both are added to `[workspace] members`. (Flat
`crates/qhyccd-rs` + `crates/libqhyccd-sys` is the alternative — see Open Q1.)

Names stay unprefixed (external-SDK bindings — per the crate-naming convention).

## Phases

### Phase 0 — Decisions (SETTLED 2026-06-17 — see "Decisions" below)
- **Publishing: dual-home**, **Layout: nested**, **Record: ADR-009**.
- Write the ADR stub `docs/decisions/009-vendor-qhyccd-rs.md` — citing ADR-001
  Amendment A (the cfitsio purge) as the native-link-in/out-of-tree precedent and
  ADR-008 (zwo-camera native SDK FFI, PR #369) as the sibling decision.

**Exit:** decisions recorded (below) + ADR-009 stub written.

### Phase 1 — Vendor on the Cargo side (no Bazel yet) — DONE (commit 81a4d42)
- Copy `qhyccd-rs` + `libqhyccd-sys` source into `crates/qhyccd-rs/…` (drop their
  `target/`, standalone `Cargo.lock`, and `.git` artefacts).
- Add both to `[workspace] members`.
- **Preserve publishability (dual-home).** Keep each vendored manifest's explicit
  `version` / `edition` / `license` / `authors` / `description` / `repository` —
  do **NOT** switch them to `*.workspace = true`; they release on their own
  cadence (qhyccd-rs 0.1.9, libqhyccd-sys 0.1.4). Keep `CHANGELOG.md` /
  `README.md` / `LICENSE-*` in each crate. Keep the
  `libqhyccd-sys = { version = "0.1.4", path = "libqhyccd-sys" }` dual dep
  verbatim — `path` drives dev/Bazel builds, `version` drives `cargo publish`.
- Delete the `[patch.crates-io]` block. Change the workspace dep
  `qhyccd-rs = "=0.1.9"` → `qhyccd-rs = { path = "crates/qhyccd-rs" }`.
- qhy-camera `Cargo.toml`: the normal `qhyccd-rs = { workspace = true }` is
  unchanged; **leave the `simulation` dev-dep in place for now** (Phase 2 decides
  its fate — see rand/rayon nuance).
- `cargo update` / regenerate `Cargo.lock`; confirm the resolved `qhyccd-rs` +
  `libqhyccd-sys` are now the path crates and `rand`/`rayon` remain locked.
- **Verify Cargo parity:**
  - `cargo build -p qhy-camera` → links the **real** SDK (the prod path is
    unchanged).
  - `cargo rail run --profile commit -q` → qhy-camera's 64 tests green.
  - `cargo test -p qhyccd-rs --features simulation` (the vendored crate's own
    suite) green — it now runs as a workspace member.

**Exit:** patch gone; workspace builds/tests via path deps; no behaviour change.

### Phase 2 — Bazel: build scripts + two variants — DONE (commit 0325417)

**Outcome (2026-06-17):** implemented as designed below. Verified on Linux:
`bazel build //...` (140 targets); the prod binary links the **real** static SDK;
`qhy-camera_unit_test` (prod), `bdd` (sim, ~16 s, 0 USB), and the full
`conformu_integration` (sim, ~33 s, ConformU 4.3.0 → **0 errors / 0 issues**) all
pass; no lockfile drift. Two notes vs. the original design:
- **rand/rayon spike → KEEP the dev-dep.** Dropping qhy-camera's
  `qhyccd-rs = { features = ["simulation"] }` dev-dep makes `@cr` lose `rand`/`rayon`
  and `:qhyccd-rs_sim` fails to compile (`unresolved import rand`). So the dev-dep
  stays; its role is now *only* "keep simulation's optional deps in `@cr`" (the SDK
  variant is chosen by the two BUILD targets, not the dev-dep). This resolves the
  plan's one open question.
- **Orphan `@cr` `libqhyccd-sys`.** Because the vendored path dep keeps a `version`
  (needed for dual-home publish), crate_universe still materializes
  `@cr__libqhyccd-sys-0.1.4`. It is an orphan — `qhyccd-rs` resolves the
  workspace-member edge (`deps(//crates/qhyccd-rs:qhyccd-rs)` references only
  `//crates/qhyccd-rs/libqhyccd-sys`), so nothing depends on it and it is never
  fetched/built. Harmless dead weight; documented rather than worked around.

Original design (as implemented):
- `crates/qhyccd-rs/libqhyccd-sys/BUILD.bazel`:
  - `cargo_build_script(name = "build_script", srcs = ["build.rs"], …)` —
    rules_rust auto-provides `CARGO_CFG_TARGET_OS`/`_ARCH`; forward
    `GITHUB_WORKSPACE` (already in `.bazelrc` for macOS/Windows) and set
    `links`-equivalent metadata. Mirror what `crate_universe` generates for it
    today (inspect `@cr__libqhyccd-sys-0.1.4//` as the reference).
  - `rust_library(name = "libqhyccd-sys", crate_name = "libqhyccd_sys",
    path = "lib.rs", deps = [":build_script"])`.
- `crates/qhyccd-rs/BUILD.bazel`:
  - `rust_library(name = "qhyccd-rs", crate_features = [],
    deps = [":libqhyccd-sys-target", "@cr//:eyre", …])` — **real** SDK.
  - `rust_library(name = "qhyccd-rs_sim", testonly = True,
    crate_name = "qhyccd_rs", crate_features = ["simulation"],
    deps = [… , "@cr//:rand", "@cr//:rayon"])` — simulated.
    `testonly = True` makes Bazel reject any production target that links it.
- qhy-camera `BUILD.bazel`:
  - Replace the `qhyccd-rs` entry from `all_crate_deps(normal=True)` with an
    explicit dep (it's no longer an `@cr` crate). Prod targets
    (`qhy-camera_lib`, `qhy-camera`, `qhy-camera_unit_test`) →
    `//crates/qhyccd-rs:qhyccd-rs`; sim targets (`_lib_sim`, `_sim`, `bdd`,
    `conformu_integration`) → `//crates/qhyccd-rs:qhyccd-rs_sim`.
  - Keep the `crate_features = [… , "simulation"]` on the sim targets (that drives
    qhy-camera's *own* cfg; still required).
- **rand/rayon availability (the one real nuance).** `@cr//:rand` and `@cr//:rayon`
  exist only if the Cargo resolution reaches `qhyccd-rs/simulation`. With
  `qhyccd-rs` now first-party, confirm whether they're still resolved into `@cr`
  without the dev-dep. If **yes**, drop the qhy-camera `simulation` dev-dep
  entirely. If **no**, the dev-dep stays but its role shrinks to "keep
  simulation's optional deps in `@cr`" (document that), or pull them via a
  `crate.annotation`. **Spike this first** (`bazel query @cr//:rayon` after a
  no-dev-dep repin) before deciding.
- `scripts/repin-bazel-lock.sh` (Rule 10), then
  `git diff --cached MODULE.bazel.lock`.
- **Verify Bazel:**
  - `bazel build //crates/qhyccd-rs/...` + `//services/qhy-camera/...`.
  - Run the **prod** binary (`bazel-bin/services/qhy-camera/qhy-camera`) →
    confirm it now calls the **real** `InitQHYCCDResource` (the parity win — strace
    or the `QHYCCD.CPP` log lines).
  - `bazel test //services/qhy-camera:qhy-camera_unit_test`,
    `:bdd --test_tag_filters=bdd`, `:conformu_integration --config=conformu` →
    all green, sim binary still simulated (0 USB).
  - `bazel build --nobuild //... --lockfile_mode=error` → no graph drift.

**Exit:** real prod / sim test under Bazel; dev-dep dropped or its role
documented.

### Phase 3 — Cleanup + docs — DOCS DONE; publish/archive PENDING
- ✅ Update docs/services/qhy-camera.md "Native dependency & build gating":
  replaced the dev-dep narrative with the first-party two-variant story; noted the
  patch is gone.
- ✅ Update docs/plans/archive/bazel-migration.md "External-crate non-default features"
  subsection: added a closing note that qhy-camera moved to the first-party
  two-variant pattern (and that the dev-dep technique remains the answer for
  crates we *don't* vendor, e.g. until zwo-rs is vendored).
- ✅ Update the memory note ([[qhy-camera-implementation]] / MEMORY.md Build
  Notes): qhyccd-rs is now first-party; no `[patch.crates-io]`; real/sim split.
- ✅ `cargo fmt`. buildifier still not on PATH — BUILD files matched to
  surrounding style by hand.
- ✅ Release runbook written: [`crates/qhyccd-rs/RELEASING.md`](../../crates/qhyccd-rs/RELEASING.md)
  (publish order, version-bump rules, the post-publish archive trigger).
- ⏳ **First `cargo publish` from the vendored subdirs** — not yet done. Follow
  RELEASING.md (`libqhyccd-sys` must bump past `0.1.4` to carry the in-tree macOS
  fix; publish it first, then `qhyccd-rs`).
- ⏳ **Hard-archive the standalone `ivonnyssen/qhyccd-rs` repo** — only *after*
  the first publish-from-subdir confirms the monorepo release path works. Point
  its README at the monorepo, then GitHub → Settings → Archive (read-only).

**Exit:** docs/memory consistent (✅); first publish-from-subdir + standalone
archive done; `cargo rail` + Bazel shadow jobs green (✅).

### Phase 4 — (future, not this plan) zwo-rs
Apply the same vendoring to `zwo-rs`/`libzwo-sys`, reusing the `cargo_build_script`
pattern — but it adds **bindgen/libclang** to the build script, so budget extra
work for the codegen action. Tracked separately by the user.

## Risks & mitigations

| Risk | Likelihood | Mitigation |
|---|---|---|
| First first-party `cargo_build_script` + native `static=qhyccd` link misbehaves under Bazel | Low | `crate_universe` already runs this exact `build.rs` under Bazel today; copy its generated target as the reference. The static SDK + `/usr/local/lib` read-mount is unchanged. |
| `rand`/`rayon` not in `@cr` after dropping the dev-dep | Medium | Spike (`bazel query @cr//:rayon`) before dropping; keep the dev-dep (role-shifted) or add a `crate.annotation` if needed. Not a blocker, just a wiring choice. |
| macOS/Windows build-script env (`GITHUB_WORKSPACE`) not forwarded to the new target | Low | Already forwarded in `.bazelrc` (`build:macos/windows --action_env=GITHUB_WORKSPACE`); `cargo_build_script` inherits it. |
| Upstream/in-tree divergence if dual-homed | Medium | Decide Open Q2 up front; if dual-homed, document the one-way sync direction. |
| Workspace feature-unification surprises (qhyccd-rs deps now unified with the whole workspace) | Low | Phase 1 verifies `cargo rail` + the vendored crate's own tests before any Bazel work. |
| `cargo publish` of qhyccd-rs becomes awkward from a subdir | Low | Only if dual-homing; `cargo publish` works from a member dir, just note it in the release runbook. |

## Rollback
Each phase is independently revertible. Phase 1 rollback = restore the
`[patch.crates-io]` block + crates.io dep and delete the vendored dirs. Phase 2
rollback = restore the dev-dep + `all_crate_deps` qhyccd-rs entry and repin.
Nothing here touches production release packaging (Cargo, unchanged).

## Decisions (Phase 0 — settled interactively 2026-06-17)
1. **Layout: nested.** `crates/qhyccd-rs/` with `libqhyccd-sys/` nested inside,
   preserving the upstream `libqhyccd-sys = { version = "0.1.4", path =
   "libqhyccd-sys" }` dep verbatim (zero edits to the vendored manifests).
2. **Publishing: dual-home.** The monorepo becomes the canonical development home,
   and we keep publishing `qhyccd-rs` / `libqhyccd-sys` to crates.io **from the
   vendored subdirs**. Clean because the dep already carries both a `version` and
   a `path` (`cargo publish` uses the version; dev/Bazel use the path). Implies:
   keep independent explicit versions + per-crate metadata + `CHANGELOG`/`README`/
   `LICENSE-*` (see Phase 1), and add a release runbook (publish `libqhyccd-sys`,
   then `qhyccd-rs`). The standalone `ivonnyssen/qhyccd-rs` repo is retired in
   favour of the monorepo as single source of truth.
3. **Record: ADR-009** (`docs/decisions/009-vendor-qhyccd-rs.md`). 008 is taken by
   zwo-camera's native-SDK ADR on PR #369; if branch merge order flips, renumber.

## Open questions
- **Drop the dev-dep entirely?** **RESOLVED (2026-06-17): no — keep it, role-shifted.**
  The Phase 2 spike confirmed `@cr` loses `rand`/`rayon` without it and `:qhyccd-rs_sim`
  fails to compile. The dev-dep is retained solely to keep simulation's optional deps
  in `@cr`; it no longer selects the SDK variant (the two BUILD targets do).

# Releasing `qhyccd-rs` and `libqhyccd-sys`

These two crates are **dual-homed** (ADR-009): the workspace is the canonical
development home, but both are still published to crates.io for outside
consumers. The inter-crate dependency carries **both** a `version` and a `path`
(`libqhyccd-sys = { version = "0.1.4", path = "libqhyccd-sys" }`) — local and
Bazel builds use the `path`; `cargo publish` rewrites it to the `version`. That
mechanic dictates the publish **order** and the version-bump rules below.

## Publish order (always)

1. **`libqhyccd-sys` first** — `qhyccd-rs` depends on it, and `cargo publish`
   verifies `qhyccd-rs` by building it against the **crates.io** copy of
   `libqhyccd-sys`. That copy must exist and be indexed before step 2.
2. **`qhyccd-rs` second.**

## Version-bump rules

- **`libqhyccd-sys` must bump past `0.1.4` before its next publish.** crates.io
  `0.1.4` was cut *before* the macOS link fix, which is why the deleted
  `[patch.crates-io]` git override existed (ADR-009, motivation #1). The fix now
  lives in the in-tree `build.rs`, so the first publish from the monorepo carries
  it — that is a real change and needs `>= 0.1.5`.
- **Bump `qhyccd-rs`** to cover its `[Unreleased]` `CHANGELOG.md` entries.
- **If `libqhyccd-sys` is bumped, bump the `version` of the dep in
  `crates/qhyccd-rs/Cargo.toml` to match** — otherwise the published `qhyccd-rs`
  will request a `libqhyccd-sys` version that doesn't exist on crates.io. The
  `path` is unaffected (in-tree builds keep working).

## MSRV

Both crates declare an **explicit, lower-than-workspace** `rust-version` (not
`workspace = true`, which would publish the workspace's `1.98.1`), and the two now
differ: **`qhyccd-rs` is `1.85.0`** (its `simulation` feature pulls rand 0.10, MSRV
1.85; the base build is held to 1.81 by derive_more 2.1) while **`libqhyccd-sys` is
`1.68.0`** (dependency-free hand-written FFI). The nightly publish-readiness check
verifies each floor builds with minimal dependency versions, and its advisory
`find` leg reports the true lowest. If a change raises the floor (a new std API, a
dependency MSRV bump), **bump that crate's `rust-version`** to the value the check
accepts; to keep it low, prefer APIs/deps available on the declared MSRV.
See [docs/plans/archive/publish-readiness-checks.md](../../docs/plans/archive/publish-readiness-checks.md).

## Steps

Do this on a release branch (never `main`), with a clean working tree and CI
green. The QHYCCD SDK must be installed at `/usr/local/lib` — `cargo publish`'s
verification build **links the real static SDK**.

```bash
# 0. Preflight
git status                      # must be clean
bazel test //...                # build + test gate
# Publish-readiness MUST be green for the crate being released — it verifies the
# published-in-isolation guarantees (MSRV, direct-minimal-versions, semver,
# docs.rs) that the in-workspace checks cannot. Trigger it and confirm it passes:
#   gh workflow run publish-readiness.yml      # or rely on the last green nightly
# A red run BLOCKS the release. See docs/plans/archive/publish-readiness-checks.md.

# 1. Bump versions + changelogs
#    - crates/qhyccd-rs/libqhyccd-sys/Cargo.toml : version = "0.1.5" (etc.)
#    - crates/qhyccd-rs/Cargo.toml               : version bump  AND
#                                                  libqhyccd-sys dep version -> "0.1.5"
#    - crates/qhyccd-rs/CHANGELOG.md             : move [Unreleased] -> [x.y.z] - <date>
#      (libqhyccd-sys has no CHANGELOG; note its change under qhyccd-rs or add one)

# 2. Publish libqhyccd-sys FIRST, then wait for the index
cargo publish -p libqhyccd-sys --dry-run
cargo publish -p libqhyccd-sys
#    wait until `cargo search libqhyccd-sys` shows the new version

# 3. Publish qhyccd-rs
cargo publish -p qhyccd-rs --dry-run        # builds against the just-published sys crate
cargo publish -p qhyccd-rs

# 4. Tag
git tag qhyccd-rs-vX.Y.Z && git push --tags
```

## After the *first* successful publish-from-subdir

This is the trigger for the last deferred ADR-009 step: **hard-archive the
standalone `ivonnyssen/qhyccd-rs` repo** (point its README at the monorepo, then
GitHub → Settings → Archive). Do not archive before a publish-from-monorepo has
succeeded — that is what proves the release path.

## Bazel

The version bump **does** need a repin. `MODULE.bazel.lock` records a hash of
`Cargo.lock` and of every workspace `Cargo.toml`, and the bump edits both
manifests and the lock. So run `scripts/repin-bazel-lock.sh` in the bump commit
(Rule 10); the pre-commit hook refuses that commit otherwise, and so does CI.
`MODULE.bazel` itself is unchanged, because workspace members are
auto-discovered.

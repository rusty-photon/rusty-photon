# Releasing `svbony-rs` and `libsvbony-sys`

These two crates are **dual-homed**, like `qhyccd-rs` and `zwo-rs`: the
workspace is the canonical development home, and both publish to crates.io
from `crates/svbony-rs`. The inter-crate dependency carries **both** a
`version` and a `path`
(`libsvbony-sys = { version = "0.1.0", path = "libsvbony-sys" }`) — local and
Bazel builds use the `path`; `cargo publish` rewrites it to the `version`. That
mechanic dictates the publish **order** and the version-bump rules below.

## Publish order (always)

1. **`libsvbony-sys` first** — `svbony-rs` depends on it, and `cargo publish`
   verifies `svbony-rs` by building it against the **crates.io** copy of
   `libsvbony-sys`. That copy must exist and be indexed before step 2.
2. **`svbony-rs` second.**

## Version-bump rules

- **Bump `svbony-rs`** to cover its `[Unreleased]` `CHANGELOG.md` entries.
- **Bump `libsvbony-sys`** when its bindings or build script changed since its
  last release.
- **If `libsvbony-sys` is bumped, bump the `version` of the dep in
  `crates/svbony-rs/Cargo.toml` to match** — otherwise the published
  `svbony-rs` will request a `libsvbony-sys` version that doesn't exist on
  crates.io. The `path` is unaffected (in-tree builds keep working).

## MSRV

Both crates inherit the **workspace MSRV** (`rust-version.workspace = true`, see
[docs/workspace.md](../../docs/workspace.md#msrv)); `cargo publish` writes the
concrete value into the published manifest. The nightly `msrv` job in
`check.yml` verifies it.

## Steps

Do this on a release branch (never `main`), with a clean working tree and CI
green.

```bash
# 0. Preflight
git status                      # must be clean
bazel build //... && bazel test //...   # build + test gate
# The version bump in step 1 must cover every API change since the last release.
# Skip this for a crate's first publish: with no crates.io baseline there is
# nothing to compare against.
cargo semver-checks --package libsvbony-sys
cargo semver-checks --package svbony-rs

# 1. Bump versions + changelog
#    - crates/svbony-rs/libsvbony-sys/Cargo.toml : version bump
#    - crates/svbony-rs/Cargo.toml               : version bump  AND
#                                                  libsvbony-sys dep version
#    - crates/svbony-rs/CHANGELOG.md             : move [Unreleased] -> [x.y.z] - <date>
#      (libsvbony-sys has no CHANGELOG; note its change under svbony-rs)

# 2. Publish libsvbony-sys FIRST, then wait for the index
cargo publish -p libsvbony-sys --dry-run
cargo publish -p libsvbony-sys
#    wait until `cargo search libsvbony-sys` shows the new version

# 3. Publish svbony-rs
cargo publish -p svbony-rs --dry-run        # builds against the just-published sys crate
cargo publish -p svbony-rs

# 4. Tag
git tag svbony-rs-vX.Y.Z && git push --tags
```

## Bazel

The version bump needs a repin. `MODULE.bazel.lock` records a hash of
`Cargo.lock` and of every workspace `Cargo.toml`, and the bump edits both
manifests and the lock. So run `scripts/repin-bazel-lock.sh` in the bump commit
(Rule 10); the pre-commit hook refuses that commit otherwise, and so does CI.

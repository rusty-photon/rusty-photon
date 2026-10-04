#!/usr/bin/env bash
# Refresh MODULE.bazel.lock after a change to Cargo.lock or any workspace
# Cargo.toml (the root's or a member's), and stage it.
#
# Bazel's crate_universe reads the Cargo files, and the lock records a hash of
# each one, so any edit to them leaves the lock stale and CI's
# `--lockfile_mode=error` rejects it (see docs/skills/pre-push.md,
# "Refreshing MODULE.bazel.lock"). The pre-commit hook catches the miss
# through tools/ci/check_bazel_lock.py; this is the fix it asks for.
#
# .github/workflows/repin-bazel.yml runs the same repin on dependabot PRs.
# Keep the two in step.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# crate_universe records these in the lock. CI sets none of them, so one left
# exported in this shell would produce a lock that passes every step below and
# is still rejected on the PR.
unset CARGO_BAZEL_DEBUG CARGO_BAZEL_GENERATOR_SHA256 CARGO_BAZEL_GENERATOR_URL \
    CARGO_BAZEL_ISOLATED CARGO_BAZEL_REPIN CARGO_BAZEL_REPIN_ONLY \
    CARGO_BAZEL_TIMEOUT REPIN

# Re-resolve the crate index against the current Cargo files.
CARGO_BAZEL_REPIN=1 bazel mod tidy

# The second, un-forced run resets the lock's recorded CARGO_BAZEL_REPIN
# fingerprint to null, so the committed lock does not churn on later plain
# `bazel` runs.
bazel mod tidy

# `bazel mod tidy` only fixes up extensions it reaches through an explicit
# `use_repo()` in MODULE.bazel, so one pulled in transitively, with no
# `use_repo()` of its own, can be left out of the lock on some hosts. A
# `--lockfile_mode=update` build does the full target-graph analysis and fills
# the gap; without it the lock can look fine here and still be rejected by
# CI's `--lockfile_mode=error` on x86_64.
bazel build --nobuild --lockfile_mode=update //...

# Resolve once more in CI's mode. On this host only: it cannot see a gap that
# shows up on another one, which is what the step above is for.
bazel build --nobuild --lockfile_mode=error //...

git add MODULE.bazel.lock

# The repin hashed the working tree, so the lock matches the Cargo files as
# they are on disk. Any of them left unstaged would make the commit disagree
# with its own lock; the generator can also have rewritten Cargo.lock itself.
# Untracked manifests count too: a new member's Cargo.toml is one until added.
unstaged=$(
    git diff --name-only -- Cargo.lock '*Cargo.toml'
    git ls-files --others --exclude-standard -- Cargo.lock '*Cargo.toml'
)
if [ -n "$unstaged" ]; then
    echo "MODULE.bazel.lock refreshed and staged, from Cargo files that are not staged:"
    printf '%s\n' "$unstaged" | sed 's/^/  /'
    echo "Stage them too (git add), or the pre-commit check will refuse the commit."
else
    echo "MODULE.bazel.lock refreshed and staged."
fi

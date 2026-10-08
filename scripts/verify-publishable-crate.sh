#!/usr/bin/env bash
#
# verify-publishable-crate.sh — isolated MSRV + minimal-versions publish-readiness
# check for one dual-homed FFI crate family (a wrapper + its nested `*-sys`).
#
# WHY THIS EXISTS (see docs/plans/archive/publish-readiness-checks.md):
#   These crates publish to crates.io independently, but their MSRV cannot be
#   verified against minimal dependency versions *in* the workspace: the shared
#   Cargo.lock pins newest deps, and `cargo update -Zminimal-versions` is a
#   whole-lockfile operation the rest of the workspace won't tolerate.
#   So we copy the family OUT of the workspace and verify it the way a crates.io
#   consumer would — on its own declared MSRV, with a minimal-versions lockfile.
#
# THE RECIPE (proven by dogfooding, 2026-06-22):
#   direct-minimal-versions floors only DIRECT deps; transitive deps float to
#   newest and can demand a higher Rust than our floor (e.g. rayon -> rayon-core
#   1.13 needs 1.80). Pairing it with the MSRV-aware resolver
#   (CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback) caps transitive deps at
#   MSRV-compatible versions, so direct-minimal-versions works on the declared floor.
#
# USAGE:
#   scripts/verify-publishable-crate.sh <wrapper-crate-name> [verify|find]
#     verify (default) — assert each crate builds on its declared MSRV with a
#                        direct-minimal-versions lockfile, across the feature powerset.
#     find             — report the lowest Rust each crate builds on today
#                        (`cargo msrv find`). Informational: every crate declares the
#                        workspace MSRV (docs/workspace.md "MSRV").
#
# REQUIREMENTS: rustup (nightly + each crate's MSRV toolchain — auto-installed),
#   jq, cargo-hack (feature powerset; falls back to default/all-features if absent),
#   cargo-msrv (find mode only). For a family with needs-libclang=true (zwo),
#   libclang must be present for bindgen; the ZWO SDK *binary* is NOT needed —
#   the skip-link env makes the build script emit no link directives and this is a
#   check-only build (never links).
#
set -euo pipefail

# Associative arrays (`declare -A`, below) need Bash >= 4. Stock macOS /bin/bash is
# 3.2; `#!/usr/bin/env bash` picks up a Homebrew bash when one is on PATH, but guard
# explicitly so an old bash fails with a clear message rather than the opaque
# `declare: -A: invalid option`.
if [ "${BASH_VERSINFO:-0}" -lt 4 ]; then
  echo "FATAL: this script needs Bash >= 4 (found ${BASH_VERSION:-unknown}). On macOS: 'brew install bash', then re-run with that bash." >&2
  exit 2
fi

# Portable in-place sed: BSD/macOS `sed -i` requires a (possibly empty) backup-suffix
# argument, so `sed -i -E …` is GNU-only — BSD sed reads `-E` as the suffix. Write
# through a temp file instead; `-E` itself is fine on both GNU (>= 4.2) and BSD.
sed_inplace() { # <file> <sed-args...>
  local file="$1"; shift
  local tmp; tmp="$(mktemp "${TMPDIR:-/tmp}/verify-pub.XXXXXX")"
  sed "$@" "$file" >"$tmp" && mv "$tmp" "$file"
}

WRAPPER="${1:?usage: verify-publishable-crate.sh <wrapper-crate-name> [verify|find]}"
MODE="${2:-verify}"

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

command -v jq >/dev/null || { echo "FATAL: jq is required" >&2; exit 2; }

# --- discover the family from [package.metadata.publish-readiness] -------------
META="$(cargo metadata --format-version 1 --no-deps)"

pkg_field() { # <crate-name> <jq-path-from-package>
  echo "$META" | jq -r --arg n "$1" ".packages[] | select(.name==\$n) | $2"
}
meta_field() { # <key under metadata.publish-readiness>
  echo "$META" | jq -r --arg n "$WRAPPER" --arg k "$1" \
    '.packages[] | select(.name==$n) | .metadata["publish-readiness"][$k]'
}

WRAPPER_MANIFEST="$(pkg_field "$WRAPPER" .manifest_path)"
if [ "$WRAPPER_MANIFEST" = "null" ] || [ -z "$WRAPPER_MANIFEST" ]; then
  echo "FATAL: '$WRAPPER' is not a workspace package" >&2; exit 2
fi

SYS="$(meta_field sys-crate)"
SKIP_ENV="$(meta_field skip-link-env)"
NEEDS_LIBCLANG="$(meta_field needs-libclang)"
if [ "$SYS" = "null" ] || [ -z "$SYS" ]; then
  echo "FATAL: '$WRAPPER' has no [package.metadata.publish-readiness] (not a publishable FFI family)" >&2
  exit 2
fi
# `skip-link-env` is load-bearing: it names the build-script var that suppresses
# native linking. A missing/misspelled key would silently degrade to `env "null=1"
# cargo …` (a junk var, real skip-link var unset → the build script tries to link
# the SDK and fails confusingly), so require it explicitly.
if [ "$SKIP_ENV" = "null" ] || [ -z "$SKIP_ENV" ]; then
  echo "FATAL: '$WRAPPER' [package.metadata.publish-readiness] is missing 'skip-link-env' (the build-script var that suppresses native linking — e.g. QHYCCD_SKIP_NATIVE_LINK)" >&2
  exit 2
fi
# `needs-libclang` is optional (absent ⇒ the family does not run bindgen). Normalize
# anything that is not the literal "true" to "false" so the later `= "true"` test is exact.
[ "$NEEDS_LIBCLANG" = "true" ] || NEEDS_LIBCLANG="false"

WRAPPER_DIR="$(dirname "$WRAPPER_MANIFEST")"
SYS_MANIFEST="$(pkg_field "$SYS" .manifest_path)"
# A misspelled `sys-crate` resolves to no package; guard so we fail clearly instead
# of running `dirname "null"` (→ ".") and copying the wrong tree.
if [ "$SYS_MANIFEST" = "null" ] || [ -z "$SYS_MANIFEST" ]; then
  echo "FATAL: sys-crate '$SYS' (from $WRAPPER's publish-readiness metadata) is not a workspace package — check the name" >&2
  exit 2
fi
SYS_DIR="$(dirname "$SYS_MANIFEST")"
SYS_SUBDIR="${SYS_DIR#"$WRAPPER_DIR"/}"   # e.g. libqhyccd-sys (nested under wrapper)

WRAPPER_MSRV="$(pkg_field "$WRAPPER" .rust_version)"
SYS_MSRV="$(pkg_field "$SYS" .rust_version)"
for v in "$WRAPPER_MSRV:$WRAPPER" "$SYS_MSRV:$SYS"; do
  [ "${v%%:*}" != "null" ] || { echo "FATAL: ${v#*:} has no rust-version (declare an explicit MSRV)" >&2; exit 2; }
done

echo "== publish-readiness: $WRAPPER (MSRV $WRAPPER_MSRV) + $SYS (MSRV $SYS_MSRV) =="
echo "   skip-link env: $SKIP_ENV=1   needs-libclang: $NEEDS_LIBCLANG   mode: $MODE"

# --- preflight: toolchains + libclang -----------------------------------------
ensure_toolchain() { rustup toolchain list | grep -q "^$1" || rustup toolchain install "$1" --profile minimal; }
ensure_toolchain nightly
ensure_toolchain "$WRAPPER_MSRV"
ensure_toolchain "$SYS_MSRV"

if [ "$NEEDS_LIBCLANG" = "true" ]; then
  # bindgen finds libclang via LIBCLANG_PATH, the ldconfig cache, common llvm lib
  # dirs, or a `clang` on PATH. Only warn if none of those turn it up.
  if [ -z "${LIBCLANG_PATH:-}" ] \
     && ! ldconfig -p 2>/dev/null | grep -qi 'libclang' \
     && ! ls /usr/lib/llvm-*/lib/libclang.so* /usr/lib/*/libclang*.so* >/dev/null 2>&1 \
     && ! command -v clang >/dev/null 2>&1; then
    echo "WARNING: needs-libclang=true but no libclang found (set LIBCLANG_PATH or install libclang); bindgen will fail." >&2
  fi
fi

# --- copy the family out of the workspace -------------------------------------
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/verify-pub.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT
cp -R "$WRAPPER_DIR" "$SCRATCH/pkg"
rm -rf "$SCRATCH/pkg/target" "$SCRATCH/pkg/Cargo.lock"

# Inline the wrapper's `{ workspace = true }` deps with the concrete versions from
# the root [workspace.dependencies] — outside the workspace there is no root to
# inherit from. (`cargo publish` does the same inlining for the real release.)
declare -A WSDEP
while IFS= read -r line; do
  name="$(printf '%s' "$line" | sed -nE 's/^([A-Za-z0-9_-]+)[[:space:]]*=.*/\1/p')"
  # `|| true`: a no-match grep returns 1, which under `set -o pipefail`/`set -e`
  # would otherwise abort the script on the first path dep (no numeric version).
  ver="$(printf '%s' "$line" | grep -oE '"[0-9][^"]*"' | head -1 | tr -d '"' || true)"
  # Skip path/workspace-internal deps (no concrete version, e.g. bdd-infra) — only
  # crates.io entries (with a numeric version) are inlinable. An `if` (not a `&&`
  # chain) so an empty version doesn't return non-zero and trip `set -e`.
  if [ -n "$name" ] && [ -n "$ver" ]; then WSDEP["$name"]="$ver"; fi
done < <(awk '/^\[workspace\.dependencies\]/{f=1;next} /^\[/{f=0} f' Cargo.toml)

for name in "${!WSDEP[@]}"; do
  # Replace only the `workspace = true` token on this dep's line, preserving any
  # sibling keys (features / optional / default-features that an inherited dep may
  # carry, e.g. `derive_more = { workspace = true, features = ["eq"] }`):
  #   { workspace = true }                 -> { version = "x" }
  #   { workspace = true, features = [..] } -> { version = "x", features = [..] }
  # Both are valid TOML and match what `cargo publish` emits for the release.
  sed_inplace "$SCRATCH/pkg/Cargo.toml" -E \
    "/^${name}[[:space:]]*=[[:space:]]*\{[^}]*workspace[[:space:]]*=[[:space:]]*true/ s|workspace[[:space:]]*=[[:space:]]*true|version = \"${WSDEP[$name]}\"|"
done
# Guard: a *dependency* line (key = { ... workspace = true ... }) still inheriting.
# Anchored to a key at line start so it never matches a `#` comment that merely
# mentions "workspace = true" (qhyccd-rs's [lints] note does).
if grep -qE '^[[:space:]]*[A-Za-z0-9_-]+[[:space:]]*=[[:space:]]*\{[^}]*workspace[[:space:]]*=[[:space:]]*true' "$SCRATCH/pkg/Cargo.toml"; then
  echo "FATAL: could not inline a workspace-inherited dependency (has it grown features? extend the inliner):" >&2
  grep -nE '^[[:space:]]*[A-Za-z0-9_-]+[[:space:]]*=[[:space:]]*\{[^}]*workspace[[:space:]]*=[[:space:]]*true' "$SCRATCH/pkg/Cargo.toml" >&2
  exit 2
fi

# --- the per-crate recipe -----------------------------------------------------
gen_minimal_lockfile() { # run in $PWD = crate dir
  env "$SKIP_ENV=1" CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS=fallback \
    cargo +nightly generate-lockfile -Z direct-minimal-versions
}

verify_crate() { # <dir> <msrv> <label>
  local dir="$1" msrv="$2" label="$3"
  ( cd "$dir"
    echo "::group::[$label] generate direct-minimal-versions lockfile (nightly + MSRV-aware resolver)"
    gen_minimal_lockfile
    echo "::endgroup::"
    echo "::group::[$label] cargo +$msrv check (feature powerset, --locked minimal versions)"
    if command -v cargo-hack >/dev/null; then
      env "$SKIP_ENV=1" cargo +"$msrv" hack --feature-powerset check --locked
    else
      echo "NOTE: cargo-hack not installed — checking default + --all-features only"
      env "$SKIP_ENV=1" cargo +"$msrv" check --locked
      env "$SKIP_ENV=1" cargo +"$msrv" check --locked --all-features
    fi
    echo "::endgroup::"
  )
}

find_crate() { # <dir> <label>
  local dir="$1" label="$2"
  command -v cargo-msrv >/dev/null || { echo "FATAL: 'find' mode needs cargo-msrv" >&2; exit 2; }
  ( cd "$dir"
    echo "::group::[$label] generate direct-minimal-versions lockfile"
    gen_minimal_lockfile
    echo "::endgroup::"
    echo "::group::[$label] cargo msrv find (lowest buildable Rust with minimal versions)"
    # `find` bisects toolchains running the command after `--` (the full command,
    # including `cargo`); the skip-link env keeps it SDK-free.
    env "$SKIP_ENV=1" cargo msrv find -- cargo check --locked
    echo "::endgroup::"
  )
}

# Verify the sys crate standalone (so ITS direct deps — e.g. zwo's bindgen
# build-dep — are floored honestly), then the wrapper (which pulls the nested sys
# via its path dep).
case "$MODE" in
  verify)
    verify_crate "$SCRATCH/pkg/$SYS_SUBDIR" "$SYS_MSRV" "$SYS"
    verify_crate "$SCRATCH/pkg"             "$WRAPPER_MSRV" "$WRAPPER"
    echo "PASS: $WRAPPER + $SYS build on their declared MSRVs with minimal direct-dep versions."
    ;;
  find)
    find_crate "$SCRATCH/pkg/$SYS_SUBDIR" "$SYS"
    find_crate "$SCRATCH/pkg"             "$WRAPPER"
    echo "DONE: the lowest Rust each crate builds on is above (informational; the declared rust-version follows the workspace MSRV)."
    ;;
  *)
    echo "FATAL: unknown mode '$MODE' (use 'verify' or 'find')" >&2; exit 2 ;;
esac

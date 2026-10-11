#!/usr/bin/env bash
# Assert the inputs Bazel's clippy (`bazel build --config=clippy`, the
# `bazel clippy` step in bazel.yml) takes on trust from the BUILD files and
# clippy.toml. None of these mistakes is an error to Bazel — each one just
# lints with less than the workspace asks for, and the step stays green:
#
#  1. Every first-party Rust target sets `lint_config`. rules_rust lints a
#     target without one with clippy's defaults and passes it, silently
#     dropping `[workspace.lints.clippy]`'s deny set. Asked of `bazel query`,
#     so macro-generated, indented and one-line rule calls are all seen. The
#     `cargo_build_script` binaries are exempt: the macro does not pass the
#     attribute through, so there is nothing to set.
#  2. A target that opts out of clippy (`noclippy` and the other tags
#     rules_rust's clippy.bzl honours) is a test shard whose crate another,
#     linted `rust_test` compiles — the one case where skipping it loses
#     nothing (bdd-infra's args-only shards). Any other opt-out drops code
#     from the lint, so it fails here and has to be argued for instead.
#  3. clippy.toml's `msrv` equals the root Cargo.toml's `rust-version`.
#     rules_rust sets no CARGO_PKG_RUST_VERSION, so the key is the only MSRV
#     Bazel's clippy sees; Cargo's clippy only warns when the two differ, and
#     a warning about configuration is not a lint `-D warnings` can deny.
#
# Run from the repository root (bazel.yml does; so can you).
set -euo pipefail

status=0
rust_kinds='kind("^rust_(library|binary|test|proc_macro|shared_library|static_library) rule$", //...)'
build_scripts='attr(generator_function, "^cargo_build_script$", //...)'
# The tags rules_rust's clippy aspect skips (clippy.bzl normalises `-` to `_`
# and lowercases before comparing).
skip_tags='attr(tags, "(?i)\b(no[-_]?clippy|no[-_]?lint)\b", //...)'

missing=$(bazel query --output=label \
  "$rust_kinds except attr(lint_config, \".\", //...) except $build_scripts")
if [[ -n "$missing" ]]; then
  printf '%s\n' "$missing"
  echo "::error::the Rust targets above have no lint_config, so Bazel's clippy lints them without the workspace deny set. Add \`lint_config = \":lints\"\` (and, in a new package, the \`extract_cargo_lints\` target named \`lints\` — copy any existing BUILD.bazel)."
  status=1
fi

skipped=$(bazel query --output=label "($rust_kinds) intersect $skip_tags")
for target in $skipped; do
  crate=""
  if [[ -n "$(bazel query --output=label "kind(\"^rust_test rule$\", $target)")" ]]; then
    crate=$(bazel query --output=label "labels(crate, $target)")
  fi
  linted=""
  if [[ -n "$crate" ]]; then
    linted=$(bazel query --output=label \
      "attr(crate, \"^${crate}\$\", kind(\"^rust_test rule$\", //...)) except $skip_tags")
  fi
  if [[ -z "$linted" ]]; then
    echo "::error::$target opts out of Bazel's clippy, but no linted rust_test compiles its crate${crate:+ ($crate)} — its code would go unlinted. Remove the tag, or argue for it here."
    status=1
  else
    echo "Not linted on its own, by tag: $target (its crate $crate is linted by $(echo "$linted" | head -1))"
  fi
done

cargo_msrv=$(sed -n 's/^rust-version = "\(.*\)"$/\1/p' Cargo.toml)
clippy_msrv=$(sed -n 's/^msrv = "\(.*\)"$/\1/p' clippy.toml)
if [[ -z "$cargo_msrv" || "$cargo_msrv" != "$clippy_msrv" ]]; then
  echo "::error::clippy.toml msrv (\"$clippy_msrv\") must equal Cargo.toml rust-version (\"$cargo_msrv\"): it is the MSRV Bazel's clippy lints against."
  status=1
fi

if [[ "$status" -eq 0 ]]; then
  echo "Every first-party Rust target sets lint_config; clippy.toml msrv = Cargo.toml rust-version = $cargo_msrv."
fi
exit "$status"

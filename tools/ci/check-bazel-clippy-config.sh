#!/usr/bin/env bash
# Assert the two inputs Bazel's clippy (`bazel build --config=clippy`, the
# `bazel clippy` step in bazel.yml) takes on trust from the BUILD files and
# clippy.toml. Neither mistake is an error to Bazel — each one just lints with
# less than the workspace asks for, and the step stays green:
#
#  1. Every first-party Rust target sets `lint_config`. rules_rust lints a
#     target without one with clippy's defaults and passes it, silently
#     dropping `[workspace.lints.clippy]`'s deny set. Asked of `bazel query`,
#     so macro-generated, indented and one-line rule calls are all seen. The
#     `cargo_build_script` binaries are exempt: the macro does not pass the
#     attribute through, so there is nothing to set.
#  2. clippy.toml's `msrv` equals the root Cargo.toml's `rust-version`.
#     rules_rust sets no CARGO_PKG_RUST_VERSION, so the key is the only MSRV
#     Bazel's clippy sees; Cargo's clippy only warns when the two differ, and
#     a warning about configuration is not a lint `-D warnings` can deny.
#
# Run from the repository root (bazel.yml does; so can you).
set -euo pipefail

status=0

missing=$(bazel query --output=label \
  'kind("^rust_(library|binary|test|proc_macro|shared_library|static_library) rule$", //...)
     except attr(lint_config, ".", //...)
     except attr(generator_function, "^cargo_build_script$", //...)')
if [[ -n "$missing" ]]; then
  printf '%s\n' "$missing"
  echo "::error::the Rust targets above have no lint_config, so Bazel's clippy lints them without the workspace deny set. Add \`lint_config = \":lints\"\` (and, in a new package, the \`extract_cargo_lints\` target named \`lints\` — copy any existing BUILD.bazel)."
  status=1
fi

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

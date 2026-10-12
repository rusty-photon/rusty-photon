#!/usr/bin/env bash
# Bazel's clippy, exactly as bazel.yml's `bazel clippy` step runs it — run the
# same script locally, from anywhere in the repository:
#
#   tools/ci/bazel-clippy.sh [extra bazel build flags...]
#
# (CI passes `--config=ci`, its remote-cache flags and, on Windows, a profile.)
#
# First it asserts the inputs Bazel's clippy takes on trust from the BUILD
# files and clippy.toml. None of these mistakes is an error to Bazel — each
# one lints with less than the workspace asks for, and the run stays green:
#
#  1. Every first-party Rust target sets `lint_config`. rules_rust lints a
#     target without one with clippy's defaults and passes it, silently
#     dropping `[workspace.lints.clippy]`'s deny set. The `cargo_build_script`
#     binaries are exempt: the macro does not pass the attribute through.
#  2. No first-party Rust target opts out of clippy with one of the tags
#     rules_rust's clippy aspect skips (`noclippy`, `no_lint`, ...). An
#     opt-out drops code from the lint without any trace in the run.
#  3. clippy.toml's `msrv`, the root Cargo.toml's `rust-version` and
#     MODULE.bazel's stable Rust pin are one version. rules_rust sets no
#     CARGO_PKG_RUST_VERSION, so the key is the only MSRV Bazel's clippy sees;
#     Cargo's clippy only warns when the first two differ, and a warning about
#     configuration is not a lint `-D warnings` can deny. The pin is the MSRV
#     so that every PR compiles with it (MODULE.bazel says why).
#
# Then it lints `//...` plus the `manual` Rust targets `//...` skips: some are
# manual only because they cannot LINK without a vendor SDK (svbony-camera's
# binary), and `--config=clippy` builds the clippy_checks output group alone,
# which never links. The lint runs whatever the checks found, so one run
# shows both a check miss and a real clippy finding, and the checks' findings
# are printed after it, where the log ends.
set -uo pipefail

# The MSRV check reads Cargo.toml and clippy.toml by relative path.
cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 2

rust_kinds='kind("^rust_(library|binary|test|proc_macro|shared_library|static_library) rule$", //...)'
build_scripts='attr(generator_function, "^cargo_build_script$", //...)'
# The tags rules_rust's clippy aspect skips; clippy.bzl lowercases a tag and
# maps `-` to `_` before comparing.
skip_tags='attr(tags, "(?i)\b(no[-_]?clippy|no[-_]?lint)\b", //...)'

# The queries take the caller's `--config`s that .bazelrc defines for `common`
# or `query`, so they resolve modules as the builds around them do (CI's
# `common:ci --lockfile_mode=error`). `bazel query` rejects the rest: build
# flags, and a `--config` defined only for `build` (`remote-cache`, `profile`).
query_flags=()
for arg in "$@"; do
  if [[ $arg == --config=* ]] && grep -Eq "^(common|query):${arg#--config=}( |$)" .bazelrc; then
    query_flags+=("$arg")
  fi
done

# Prints the labels a query matches. Every call sits inside `$(...)`, where
# an `exit` would leave only that subshell, so it returns 2 and each caller
# exits on it.
query() {
  bazel query ${query_flags[@]+"${query_flags[@]}"} --output=label "$1" || {
    echo "::error::bazel query failed: $1" >&2
    return 2
  }
}

status=0
# The checks' findings, held back until the lint has run so they are the last
# thing in the log rather than buried above the build's output.
report=''
flag() {
  local line
  for line in "$@"; do report+="$line"$'\n'; done
  status=1
}

missing=$(query "$rust_kinds except attr(lint_config, \".\", //...) except $build_scripts") || exit 2
if [[ -n "$missing" ]]; then
  flag "$missing" "::error::the Rust targets above have no lint_config, so Bazel's clippy lints them without the workspace deny set. Add \`lint_config = \":lints\"\` (and, in a new package, the \`extract_cargo_lints\` target named \`lints\` — copy any existing BUILD.bazel)."
fi

skipped=$(query "($rust_kinds) intersect $skip_tags") || exit 2
if [[ -n "$skipped" ]]; then
  flag "$skipped" "::error::the Rust targets above opt out of Bazel's clippy by tag, so their code goes unlinted. Remove the tag and fix what clippy reports instead."
fi

cargo_msrv=$(sed -n 's/^rust-version = "\(.*\)"$/\1/p' Cargo.toml)
clippy_msrv=$(sed -n 's/^msrv = "\(.*\)"$/\1/p' clippy.toml)
# The one bare version in rust.toolchain's `versions` list; the nightly entry
# and the `sha256s` keys never match.
bazel_pin=$(sed -n 's/^ *"\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)",$/\1/p' MODULE.bazel)
if [[ -z "$cargo_msrv" || "$cargo_msrv" != "$clippy_msrv" || "$cargo_msrv" != "$bazel_pin" ]]; then
  flag "::error::Cargo.toml rust-version (\"$cargo_msrv\"), clippy.toml msrv (\"$clippy_msrv\") and MODULE.bazel's stable Rust pin (\"$bazel_pin\") must be one version: the pin is the MSRV, so every PR compiles with it, and clippy.toml's key is the only MSRV Bazel's clippy sees."
fi

manual=()
labels=$(query "attr(tags, \"\\bmanual\\b\", $rust_kinds) except $build_scripts") || exit 2
if [[ -n "$labels" ]]; then
  while IFS= read -r label; do manual+=("$label"); done <<<"$labels"
fi

# `${manual[@]+...}`: bash before 4.4 (macOS's /bin/bash is 3.2) treats an
# empty array as unbound under `set -u`.
bazel build --config=clippy "$@" -- //... ${manual[@]+"${manual[@]}"} || status=1

if [[ -n "$report" ]]; then
  printf '%s' "$report"
elif [[ "$status" -eq 0 ]]; then
  echo "Bazel clippy clean: every first-party Rust target linted (${#manual[@]} manual target(s) named explicitly), with the workspace lint set and MSRV $cargo_msrv."
fi
exit "$status"

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
#  3. The MSRV and MODULE.bazel's toolchain pins agree:
#     - clippy.toml's `msrv` equals the root Cargo.toml's `rust-version`.
#       rules_rust sets no CARGO_PKG_RUST_VERSION, so the key is the only MSRV
#       Bazel's clippy sees. Cargo's clippy only warns when the two differ,
#       and a warning about configuration is not a lint `-D warnings` can
#       deny.
#     - The stable Rust pin is the MSRV's release, at or above the MSRV's
#       patch level, so every PR compiles with the MSRV; MODULE.bazel says
#       why.
#     - The coverage nightly's recorded rustc version, the line directly above
#       its entry, names that date and the MSRV's release or the next one, so
#       a bump cannot leave the nightly behind. The next cycle is allowed
#       because MODULE.bazel names it as the remedy for a stabilization the
#       MSRV's last nightly lacks. The check shows the nightly's cycle, not
#       that it is the right date in it; choosing that is the bump's job.
#     - `sha256s` holds exactly the archives `cargo ci rust-toolchain-sha256s`
#       prints for the two pins. A pin bumped without its hashes would
#       otherwise download unverified.
#
# This header is the one list of these checks; the docs point here.
#
# Then it lints `//...` plus the `manual` Rust targets `//...` skips. Some are
# manual only because they cannot LINK without a vendor SDK (svbony-camera's
# binary), and `--config=clippy` builds the clippy_checks output group alone,
# which never links. The `cargo_build_script` build.rs binaries are manual
# too: rules_rust tags them so. They are linted with clippy's defaults, since
# they have no lint_config. The lint runs whatever the checks found, so one run
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
if [[ -z "$cargo_msrv" || "$cargo_msrv" != "$clippy_msrv" ]]; then
  flag "::error::clippy.toml msrv (\"$clippy_msrv\") must equal Cargo.toml rust-version (\"$cargo_msrv\"): it is the only MSRV Bazel's clippy sees."
fi

# The release a version names: 1.99 for `1.99`, `1.99.0` or `1.99.1`.
release_of() {
  [[ $1 =~ ^([0-9]+\.[0-9]+)(\.[0-9]+)?$ ]] && echo "${BASH_REMATCH[1]}"
}
# Its patch level: 1 for `1.99.1`, 0 for `1.99.0` or `1.99`.
patch_of() {
  if [[ $1 =~ ^[0-9]+\.[0-9]+\.([0-9]+)$ ]]; then echo "${BASH_REMATCH[1]}"; else echo 0; fi
}

# rust.toolchain's call in MODULE.bazel, from its opening line to its closing
# parenthesis. Its `versions` list holds the stable pin (a bare X.Y.Z) and
# the coverage nightly, one per line, the nightly directly under the line the
# hash tool prints for it: `# nightly/<date> is rustc <version>-nightly (...)`.
toolchain=$(sed -n '/^rust\.toolchain($/,/^)$/p' MODULE.bazel)
bazel_pin=$(sed -n 's/^ *"\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)",$/\1/p' <<<"$toolchain")
nightly=$(sed -n 's/^ *"nightly\/\([0-9][0-9-]*\)",$/\1/p' <<<"$toolchain")
recorded=$(awk '/^ *"nightly\/[0-9-]*",$/ { print prev } { prev = $0 }' <<<"$toolchain" \
  | sed -n 's/^ *# nightly\/\([0-9-]*\) is rustc \([0-9][0-9.]*\)-nightly .*/\1 \2/p')
msrv_release=$(release_of "$cargo_msrv")
if [[ -z "$bazel_pin" || "$bazel_pin" == *$'\n'* || -z "$nightly" || "$nightly" == *$'\n'* ]]; then
  flag "::error::could not read one stable pin and one nightly from rust.toolchain's \`versions\` in MODULE.bazel (read \"$bazel_pin\" and \"$nightly\"); keep one entry per line."
else
  # Each check reports on its own, so one run names every mismatch a bump
  # left behind.
  if [[ -z "$msrv_release" || "$(release_of "$bazel_pin")" != "$msrv_release" ]] \
    || (($(patch_of "$bazel_pin") < $(patch_of "$cargo_msrv"))); then
    flag "::error::MODULE.bazel's stable Rust pin ($bazel_pin) must be the MSRV's release, at or above the MSRV itself (Cargo.toml rust-version \"$cargo_msrv\"), so every PR compiles with the MSRV. Bump them together."
  fi
  # The nightly's cycle: the MSRV's (its last nightly is the rule) or the
  # next one (the remedy when a late stabilization is missing from it).
  # Whether it is the cycle's last nightly is the bump's job (MODULE.bazel).
  if [[ -n "$msrv_release" ]]; then
    next_release="${msrv_release%%.*}.$((${msrv_release#*.} + 1))"
    recorded_release=$(release_of "${recorded#* }")
    if [[ "${recorded%% *}" != "$nightly" || ("$recorded_release" != "$msrv_release" && "$recorded_release" != "$next_release") ]]; then
      flag "::error::the coverage nightly must come from the MSRV's ($msrv_release) cycle, or the next one's ($next_release), and the line directly above its entry must record that; for nightly/$nightly it reads \"${recorded:-no recorded version}\". Paste the line \`cargo ci rust-toolchain-sha256s nightly/<date>\` prints."
    fi
  fi
  # The archives `cargo ci rust-toolchain-sha256s` prints for the two pins:
  # its seven tools for each of its five hosts, plus rust-src, whose key has
  # no date even for the nightly (rules_rust looks it up without one). Keep
  # these lists and key shapes in step with `Release::archives` in
  # tools/rusty-photon-ci/src/rust_toolchain_sha256s.rs.
  expected=$(
    for tool in rustc cargo clippy rustfmt llvm-tools rust-std rust-analyzer; do
      for host in aarch64-apple-darwin aarch64-unknown-linux-gnu x86_64-apple-darwin \
        x86_64-pc-windows-msvc x86_64-unknown-linux-gnu; do
        echo "$tool-$bazel_pin-$host.tar.xz"
        echo "$nightly/$tool-nightly-$host.tar.xz"
      done
    done
    echo "rust-src-$bazel_pin.tar.xz"
    echo "rust-src-nightly.tar.xz"
  )
  keys=$(grep -o '"[^"]*\.tar\.xz"' <<<"$toolchain" | tr -d '"')
  missing=$(comm -23 <(sort <<<"$expected") <(sort <<<"$keys"))
  unexpected=$(comm -13 <(sort <<<"$expected") <(sort <<<"$keys"))
  if [[ -n "$missing" || -n "$unexpected" ]]; then
    flag ${missing:+"missing:"} ${missing:+"$missing"} ${unexpected:+"unexpected:"} ${unexpected:+"$unexpected"} \
      "::error::MODULE.bazel's sha256s must hold exactly the archives of the stable pin ($bazel_pin) and of the coverage nightly ($nightly), as \`cargo ci rust-toolchain-sha256s <release>\` prints them; the keys above are missing or unexpected. Regenerate them with it, or the toolchains download unverified."
  fi
fi

manual=()
labels=$(query "attr(tags, \"\\bmanual\\b\", $rust_kinds)") || exit 2
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

#!/usr/bin/env python3
"""Assert every first-party Rust target carries the crate's lint set.

Bazel's clippy (`bazel build --config=clippy`, the `bazel clippy` step in
bazel.yml) takes its deny set from each target's `lint_config` — an
`extract_cargo_lints` that reads the crate's Cargo.toml `[lints]`. A target
without one is not an error to Bazel: rules_rust lints it with clippy's
defaults and passes it. So a new crate, or a new test target in an old one,
that leaves the attribute off silently drops `unwrap_used`, `panic`,
`indexing_slicing` and the rest of `[workspace.lints.clippy]` from the Bazel
gate, and nothing announces it — the step just goes on being green.

Checked here instead, over every BUILD.bazel at once: each `rust_library`,
`rust_binary`, `rust_test` and `rust_proc_macro` call must set `lint_config`.
(`cargo_build_script` is exempt — the macro does not pass the attribute to
its script binary, so there is nothing to set.)

Runs in the `stable / clippy` job beside check_bdd_runners.py. Also runnable
locally with no arguments — the repo root is derived from this file's
location.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# Directories the walk never descends into: build outputs, Bazel's
# convenience symlinks (which republish the tree under itself), and the
# agent worktrees, each of which is a second checkout of this same repo.
PRUNED = frozenset(
    {".git", ".claude", "target", "bazel-bin", "bazel-out", "bazel-testlogs"}
)

RULE_CALL = re.compile(r"^(rust_library|rust_binary|rust_test|rust_proc_macro)\(")
LINT_CONFIG = re.compile(r"^\s*lint_config\s*=")
NAME = re.compile(r'^\s*name\s*=\s*"([^"]*)"')


def build_files(root_dir: Path) -> list[Path]:
    """Every BUILD.bazel in the repo, sorted, build outputs aside."""
    found = []
    stack = [root_dir]
    while stack:
        current = stack.pop()
        for child in current.iterdir():
            if child.is_symlink():
                continue
            if child.is_dir():
                if child.name not in PRUNED and not child.name.startswith("bazel-"):
                    stack.append(child)
            elif child.name == "BUILD.bazel":
                found.append(child)
    return sorted(found)


def targets(source: str) -> list[tuple[str, str, bool]]:
    """(rule, name, has lint_config) for each Rust rule call in a BUILD file.

    Calls are found at column 0, the only place buildifier leaves them, and
    followed to their closing parenthesis. `#` comments are dropped first so
    a commented-out `lint_config` cannot satisfy the check; a `#` inside a
    string literal would cost that line, which can only lose a match.
    """
    lines = [line.split("#", 1)[0] for line in source.splitlines()]
    found = []
    i = 0
    while i < len(lines):
        call = RULE_CALL.match(lines[i])
        if not call:
            i += 1
            continue
        depth = 0
        name = "?"
        has_lint_config = False
        j = i
        while j < len(lines):
            line = lines[j]
            if depth == 1:
                has_lint_config = has_lint_config or bool(LINT_CONFIG.match(line))
                named = NAME.match(line)
                if named and name == "?":
                    name = named.group(1)
            depth += line.count("(") + line.count("[") + line.count("{")
            depth -= line.count(")") + line.count("]") + line.count("}")
            j += 1
            if depth <= 0:
                break
        found.append((call.group(1), name, has_lint_config))
        i = j
    return found


def main() -> int:
    root_dir = Path(__file__).resolve().parents[2]
    checked = 0
    failures: list[str] = []
    for build in build_files(root_dir):
        package = build.parent.relative_to(root_dir).as_posix()
        for rule, name, has_lint_config in targets(build.read_text(encoding="utf-8")):
            checked += 1
            if not has_lint_config:
                failures.append(f"  //{package}:{name} ({rule})")

    if checked == 0:
        print(f"::error::no Rust targets found under {root_dir} — the walk is broken")
        return 1

    if failures:
        for line in failures:
            print(line)
        print(
            f"::error::{checked} Rust target(s) checked; the ones named above have "
            f"no `lint_config`, so Bazel's clippy lints them without the workspace "
            f'deny set. Add `lint_config = ":lints"` (and, in a new package, the '
            f"`extract_cargo_lints` target named `lints` — copy any existing "
            f"BUILD.bazel)."
        )
        return 1

    print(f"{checked} Rust target(s) set lint_config.")
    return 0


if __name__ == "__main__":
    sys.exit(main())

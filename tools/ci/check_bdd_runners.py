#!/usr/bin/env python3
"""Assert every BDD entry point can actually report a red suite.

Two cucumber defaults let a `harness = false` suite exit 0 while proving
nothing, and both are invisible in the summary a passing run prints:

- A step that matches no step definition is `Skipped`, and skipped passes.
  The scenario stops there, its `When`/`Then` never run, and the suite still
  reports it as a scenario. `.fail_on_skipped()` turns that into a failure.
- The bare `run` / `filter_run` runners return the summary instead of
  exiting on it, so a genuinely failed scenario leaves the binary at exit 0.
  The `_and_exit` variants are the ones that fail the build.

Neither is something a reviewer reliably notices missing from a new
`bdd.rs`, and neither announces itself afterwards — the suite just goes on
being green. So they are checked here instead, over every entry point at
once. See docs/skills/testing.md sections 2.7 and 2.9 for the rules and the
history behind them.

Runs in the `stable / clippy` job, so a new suite meets the policy on the
same required PR gate that lints it. Also runnable locally with no
arguments — the repo root is derived from this file's location.
"""

from __future__ import annotations

import sys
from pathlib import Path

RULES = "docs/skills/testing.md sections 2.7 and 2.9"

# Directories the walk never descends into: build outputs, Bazel's
# convenience symlinks (which republish the tree under itself), and the
# agent worktrees, each of which is a second checkout of this same repo.
PRUNED = frozenset(
    {".git", ".claude", "target", "bazel-bin", "bazel-out", "bazel-testlogs"}
)

RUNNERS = ("run_and_exit(", "filter_run_and_exit(")


def entry_points(root_dir: Path) -> list[Path]:
    """Every `<crate>/tests/bdd.rs` in the repo, sorted, build outputs aside."""
    found = []
    stack = [root_dir]
    while stack:
        current = stack.pop()
        for child in current.iterdir():
            if child.is_symlink():
                continue
            if child.is_dir():
                if child.name not in PRUNED:
                    stack.append(child)
            elif child.name == "bdd.rs" and child.parent.name == "tests":
                found.append(child)
    return sorted(found)


def code_of(path: Path) -> str:
    """The file with `//` line comments dropped, so prose cannot satisfy a check.

    Crude on purpose: a `//` inside a string literal would take the rest of
    that line with it. No entry point has one, and losing a line of a string
    can only cost a match, never invent one.
    """
    lines = (line.split("//", 1)[0] for line in path.read_text().splitlines())
    return "\n".join(lines)


def faults(source: str) -> list[str]:
    """Name what this entry point is missing, if anything."""
    missing = []
    if ".fail_on_skipped()" not in source:
        missing.append(
            "  no .fail_on_skipped() — an unmatched step would pass silently"
        )
    if not any(runner in source for runner in RUNNERS):
        missing.append(
            "  no run_and_exit / filter_run_and_exit — a failed scenario would exit 0"
        )
    return missing


def main() -> int:
    root_dir = Path(__file__).resolve().parents[2]
    suites = entry_points(root_dir)
    if not suites:
        print(f"::error::no tests/bdd.rs found under {root_dir} — the walk is broken")
        return 1

    failures: list[str] = []
    for suite in suites:
        missing = faults(code_of(suite))
        if missing:
            failures.append(str(suite.relative_to(root_dir)))
            failures.extend(missing)

    if failures:
        for line in failures:
            print(line)
        print(
            f"::error::{len(suites)} BDD entry point(s) checked; the ones named "
            f"above can report a green suite that proved nothing. Add the "
            f"missing call to the `Cucumber` builder ({RULES})."
        )
        return 1

    print(f"{len(suites)} BDD entry point(s) fail on skipped steps and exit on failure:")
    for suite in suites:
        print(f"  {suite.relative_to(root_dir)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

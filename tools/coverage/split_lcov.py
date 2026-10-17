#!/usr/bin/env python3
"""Split a combined LCOV coverage report into one file per workspace package.

`bazel coverage --combined_report=lcov` emits a single merged LCOV report
covering every first-party crate. The Cargo coverage job, by contrast, emits
one report per package (`cargo llvm-cov report --package <pkg>`) and uploads
each to Codecov under its own flag so the per-service badges work. To keep that
behaviour under Bazel we split the combined report here: every LCOV record is
routed to a package by the `crates/<pkg>/`, `services/<pkg>/` or
`tools/<pkg>/` segment of its
`SF:` source path (the directory basename is the Cargo package name throughout
this workspace), producing `lcov-<pkg>.info` files ready to upload with
`--flag <pkg>` (or `--flag bazel-<pkg>` in shadow mode).

Records whose source path is under none of those (generated
files, or external crates that slipped past the instrumentation filter) are
dropped -- they have no package flag. Test-file records (tests/, mock_*/test_*
bins) are kept; Codecov's `ignore:` rules drop them uniformly, exactly as for
the Cargo-produced reports.
"""

from __future__ import annotations

import argparse
import os
import re
import sys
from collections import defaultdict

# Match .../crates/<pkg>/..., .../services/<pkg>/... or .../tools/<pkg>/...
# anywhere in the path,
# tolerating absolute, workspace-relative, or bazel-out-prefixed SF: paths.
_PKG_RE = re.compile(r"(?:^|/)(?:crates|services|tools)/([^/]+)/")


def split(report_path: str, output_dir: str) -> "dict[str, int]":
    """Split ``report_path`` into ``lcov-<pkg>.info`` files in ``output_dir``.

    Returns a mapping of package name -> number of LCOV records written.
    """
    with open(report_path, "r", encoding="utf-8", errors="replace") as fh:
        text = fh.read()

    # LCOV records are delimited by `end_of_record`; each opens with an `SF:`
    # source-file line. We route a whole record on its first SF: path.
    buckets: "dict[str, list[str]]" = defaultdict(list)
    record: "list[str]" = []
    pkg_for_record: "str | None" = None

    def flush() -> None:
        nonlocal record, pkg_for_record
        if record and pkg_for_record is not None:
            buckets[pkg_for_record].append("".join(record))
        record = []
        pkg_for_record = None

    for line in text.splitlines(keepends=True):
        record.append(line)
        if pkg_for_record is None and line.startswith("SF:"):
            match = _PKG_RE.search(line[3:])
            if match:
                pkg_for_record = match.group(1)
        if line.startswith("end_of_record"):
            flush()
    flush()  # tolerate a missing trailing end_of_record

    os.makedirs(output_dir, exist_ok=True)
    counts: "dict[str, int]" = {}
    for pkg, records in sorted(buckets.items()):
        out_path = os.path.join(output_dir, f"lcov-{pkg}.info")
        with open(out_path, "w", encoding="utf-8") as fh:
            fh.write("".join(records))
        counts[pkg] = len(records)
    return counts


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(
        description="Split a combined LCOV report into per-package lcov-<pkg>.info files."
    )
    parser.add_argument("report", help="path to the combined LCOV report (.dat/.info)")
    parser.add_argument(
        "--output-dir",
        default=".",
        help="directory to write lcov-<pkg>.info files into (default: cwd)",
    )
    args = parser.parse_args(argv)

    if not os.path.isfile(args.report):
        print(f"error: report not found: {args.report}", file=sys.stderr)
        return 1

    counts = split(args.report, args.output_dir)
    if not counts:
        print("warning: no first-party (crates/|services/|tools/) records found", file=sys.stderr)
        return 0

    total = sum(counts.values())
    print(f"wrote {len(counts)} package report(s), {total} record(s) total:")
    for pkg, count in sorted(counts.items()):
        print(f"  {pkg}: {count}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Drop the coverage-exempt files from an LCOV report.

The combined `bazel coverage` report includes every instrumented first-party
target, which sweeps in a handful of files the coverage policy exempts: mock
binaries, test-helper binaries, and examples — the only sanctioned
exclusions; production code is never exempt. Coveralls computes the
published number from exactly what it is sent, so this filter applies the
policy to the upload. The CI artifact keeps the unfiltered report as
line-level truth.

The exemption pattern lives in `uncovered_in_diff.py` (``IGNORED_RE``) so
the upload and the diff scorer cannot drift apart.
"""

from __future__ import annotations

import argparse
import sys

from uncovered_in_diff import IGNORED_RE


def filter_records(text: str) -> "tuple[str, int, int]":
    """Return (filtered LCOV text, kept records, dropped records).

    An LCOV record runs from its ``SF:`` line through ``end_of_record``; the
    whole record is dropped when its source path matches the exemption
    pattern.
    """
    out: "list[str]" = []
    record: "list[str]" = []
    kept = 0
    dropped = 0
    for line in text.splitlines(keepends=True):
        record.append(line)
        if line.strip() == "end_of_record":
            source = next((l[3:].strip() for l in record if l.startswith("SF:")), "")
            if IGNORED_RE.search(source):
                dropped += 1
            else:
                kept += 1
                out.extend(record)
            record = []
    # Trailing lines outside any record (there are normally none) pass through.
    out.extend(record)
    return "".join(out), kept, dropped


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(
        description="Drop coverage-exempt files (mock/test-helper bins, examples) from an LCOV report."
    )
    parser.add_argument("lcov", help="the LCOV report to filter")
    parser.add_argument(
        "--output",
        required=True,
        help="write the filtered report here",
    )
    args = parser.parse_args(argv)

    with open(args.lcov, "r", encoding="utf-8", errors="replace") as fh:
        filtered, kept, dropped = filter_records(fh.read())
    with open(args.output, "w", encoding="utf-8") as fh:
        fh.write(filtered)
    print(f"kept {kept} record(s), dropped {dropped} exempt record(s)", file=sys.stderr)
    if kept == 0:
        print("error: the filtered report is empty", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

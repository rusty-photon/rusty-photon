#!/usr/bin/env python3
"""Attach uncovered-line annotations to a commit as a GitHub check run.

Takes the JSON report `diff-cover --format json:…` produced and creates a
completed `uncovered-diff-lines` check run on the given commit. GitHub
renders each annotation inline in the PR's Files changed tab — the
GitHub-native answer to "show me what this diff leaves uncovered", with no
coverage vendor involved. diff-cover does the scoring (diff ∩ LCOV); this
script only converts its findings into Checks API payloads and posts them.

One finding diff-cover cannot report is recovered here: a changed
first-party `.rs` file *absent from the coverage report entirely* — nothing
instrumented it, which for shipping code is a stronger finding than a few
missed lines. diff-cover silently skips such files, so this script re-checks
the diff's changed files against the report's `SF:` paths (`--diff` +
`--lcov`), skipping the coverage-exempt files per `filter_lcov.IGNORED_RE`.

The check never gates: its conclusion is `success` when every added line is
covered (or the diff has no coverable lines) and `neutral` otherwise. Making
it required would be a branch-protection decision, not a flag here.

Only GitHub Apps can create check runs, so this needs an Actions
`GITHUB_TOKEN` (the github-actions app); a user PAT cannot exercise it —
use `--dry-run` to inspect the payloads locally. In CI any API failure
prints a workflow warning and exits 0 -- annotations are a visibility
feature and must never redden the required coverage job. The one caller
that hits this by design is a fork PR, whose token is read-only.

The API accepts at most 50 annotations per request, so larger sets are
appended through update requests against the same check run. The total is
capped; past it the summary reports how many findings went un-annotated.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request

from filter_lcov import IGNORED_RE

# GitHub rejects requests carrying more than 50 annotations.
_CHUNK = 50
# A diff uncovered enough to exceed this needs a test-writing session, not a
# longer listing; the summary counts what the cap cut.
_CAP = 200

# `+++ b/path` — the post-image path of a git diff header (`/dev/null` for
# deletions), and `@@ -a,b +start,count @@` — a hunk's added-line range.
_PLUS_FILE_RE = re.compile(r"^\+\+\+ (?:b/)?(.+)$")
_HUNK_RE = re.compile(r"^@@ -\S+ \+(\d+)(?:,(\d+))? @@")


def _api(method: str, url: str, token: str, body: "dict[str, object]") -> "dict[str, object]":
    request = urllib.request.Request(
        url,
        data=json.dumps(body).encode("utf-8"),
        method=method,
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "Content-Type": "application/json",
            "User-Agent": "rusty-photon-coverage-annotations",
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def _line_runs(numbers: "list[int]") -> "list[tuple[int, int]]":
    """Collapse sorted line numbers into inclusive ``(start, end)`` runs."""
    runs: "list[tuple[int, int]]" = []
    for number in numbers:
        if runs and number == runs[-1][1] + 1:
            runs[-1] = (runs[-1][0], number)
        else:
            runs.append((number, number))
    return runs


def _first_added_lines(diff_text: str) -> "dict[str, int]":
    """Map each file the diff adds lines to onto its first added line."""
    first: "dict[str, int]" = {}
    current: "str | None" = None
    for line in diff_text.splitlines():
        file_match = _PLUS_FILE_RE.match(line)
        if file_match:
            path = file_match.group(1).strip()
            current = None if path == "/dev/null" else path
            continue
        hunk_match = _HUNK_RE.match(line)
        if hunk_match and current is not None and current not in first:
            count = int(hunk_match.group(2)) if hunk_match.group(2) else 1
            if count > 0:
                first[current] = int(hunk_match.group(1))
    return first


def _uninstrumented(diff_text: str, lcov_paths: "list[str]") -> "dict[str, int]":
    """Changed first-party `.rs` files with no record in the report at all.

    LCOV `SF:` paths may be absolute or prefixed, so a diff path is matched
    by suffix. Coverage-exempt files are skipped — their absence is policy.
    """
    sources: "set[str]" = set()
    for lcov_path in lcov_paths:
        with open(lcov_path, "r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                if line.startswith("SF:"):
                    sources.add(line[3:].strip())

    def in_report(path: str) -> bool:
        return any(sf == path or sf.endswith("/" + path) for sf in sources)

    return {
        path: line
        for path, line in _first_added_lines(diff_text).items()
        if path.endswith(".rs") and not IGNORED_RE.search(path) and not in_report(path)
    }


def _annotations(
    src_stats: "dict[str, dict[str, object]]", uninstrumented: "dict[str, int]"
) -> "list[dict[str, object]]":
    annotations: "list[dict[str, object]]" = []
    for path in sorted(src_stats):
        violations = sorted(int(n) for n in src_stats[path].get("violation_lines", []))
        for start, end in _line_runs(violations):
            if start == end:
                message = "Added by this change, but no test in the coverage run executes it."
            else:
                message = (
                    f"These {end - start + 1} added lines are not executed "
                    "by any test in the coverage run."
                )
            annotations.append(
                {
                    "path": path,
                    "start_line": start,
                    "end_line": end,
                    "annotation_level": "warning",
                    "message": message,
                }
            )
    for path in sorted(uninstrumented):
        annotations.append(
            {
                "path": path,
                "start_line": uninstrumented[path],
                "end_line": uninstrumented[path],
                "annotation_level": "warning",
                "message": (
                    "No coverage record exists for this changed file: nothing "
                    "instrumented it, so nothing in it is tested."
                ),
            }
        )
    return annotations


def _title(
    lines: int, violations: int, percent: float, uninstrumented: int
) -> "tuple[str, str]":
    """Return the check run's (title, conclusion) for diff-cover's totals."""
    if violations == 0 and uninstrumented == 0:
        if lines == 0:
            return "no coverable lines in this diff", "success"
        plural = "s" if lines != 1 else ""
        return f"every added line is covered ({lines} line{plural})", "success"
    parts = []
    if violations:
        parts.append(f"{violations} uncovered added line{'s' if violations != 1 else ''}")
    if uninstrumented:
        parts.append(
            f"{uninstrumented} changed file{'s' if uninstrumented != 1 else ''} "
            "with no coverage record"
        )
    title = ", ".join(parts)
    if lines:
        title = f"{percent:.1f}% of added lines covered — {title}"
    return title, "neutral"


def _summary(truncated: int) -> str:
    lines = [
        "Scored by `diff-cover` from this run's filtered `bazel coverage` "
        "report intersected with the PR diff. Each annotation marks added "
        "lines no test executed; they render inline in the **Files changed** "
        "tab.",
        "",
        "This check is informational and never blocks a merge.",
        "",
        "Reproduce locally: download the `bazel-coverage-lcov` artifact, "
        "filter it (`python3 tools/coverage/filter_lcov.py <artifact-dir>/*.info "
        "--output combined.info`), then run `diff-cover combined.info "
        "--compare-branch origin/main` (see docs/skills/coverage.md).",
    ]
    if truncated:
        lines += [
            "",
            f"**{truncated} further finding{'s' if truncated != 1 else ''} not "
            f"annotated** — the annotation cap ({_CAP}) was reached; the full "
            "list is in the coverage job's log and artifact.",
        ]
    return "\n".join(lines)


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(
        description="Post diff-cover's uncovered-line findings as a GitHub check run."
    )
    parser.add_argument(
        "report",
        help="the JSON report written by diff-cover --format json:…",
    )
    parser.add_argument(
        "--diff",
        required=True,
        help="the unified diff diff-cover scored (for the no-coverage-record finding)",
    )
    parser.add_argument(
        "--lcov",
        required=True,
        nargs="+",
        help="the LCOV report(s) diff-cover scored against",
    )
    parser.add_argument("--repo", required=True, help="owner/repo to post to")
    parser.add_argument("--sha", required=True, help="commit to attach the check run to")
    parser.add_argument(
        "--check-name",
        default="uncovered-diff-lines",
        help="check run name (default: uncovered-diff-lines)",
    )
    parser.add_argument(
        "--details-url",
        default=None,
        help="link the check run to this URL (typically the workflow run)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the API payloads instead of posting them",
    )
    args = parser.parse_args(argv)

    with open(args.report, "r", encoding="utf-8") as fh:
        report = json.load(fh)
    with open(args.diff, "r", encoding="utf-8", errors="replace") as fh:
        diff_text = fh.read()

    uninstrumented = _uninstrumented(diff_text, args.lcov)
    annotations = _annotations(report["src_stats"], uninstrumented)
    truncated = max(0, len(annotations) - _CAP)
    annotations = annotations[:_CAP]

    title, conclusion = _title(
        int(report["total_num_lines"]),
        int(report["total_num_violations"]),
        float(report["total_percent_covered"]),
        len(uninstrumented),
    )
    summary = _summary(truncated)

    chunks = [annotations[i : i + _CHUNK] for i in range(0, len(annotations), _CHUNK)]
    output: "dict[str, object]" = {"title": title, "summary": summary}
    if chunks:
        output["annotations"] = chunks[0]
    create_body: "dict[str, object]" = {
        "name": args.check_name,
        "head_sha": args.sha,
        "status": "completed",
        "conclusion": conclusion,
        "output": output,
    }
    if args.details_url:
        create_body["details_url"] = args.details_url
    update_bodies = [
        {"output": {"title": title, "summary": summary, "annotations": chunk}}
        for chunk in chunks[1:]
    ]

    if args.dry_run:
        print(json.dumps({"create": create_body, "updates": update_bodies}, indent=2))
        return 0

    token = os.environ.get("GITHUB_TOKEN", "")
    if not token:
        print("error: GITHUB_TOKEN is not set (use --dry-run locally)", file=sys.stderr)
        return 2
    base = os.environ.get("GITHUB_API_URL", "https://api.github.com").rstrip("/")

    try:
        created = _api("POST", f"{base}/repos/{args.repo}/check-runs", token, create_body)
        for body in update_bodies:
            _api("PATCH", f"{base}/repos/{args.repo}/check-runs/{created['id']}", token, body)
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace")[:300]
        print(f"::warning::posting {args.check_name} failed: HTTP {error.code} {detail}")
        return 0
    except urllib.error.URLError as error:
        print(f"::warning::posting {args.check_name} failed: {error.reason}")
        return 0

    print(
        f"{args.check_name}: {conclusion} ({len(annotations)} annotation(s)) "
        f"{created.get('html_url', '')}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

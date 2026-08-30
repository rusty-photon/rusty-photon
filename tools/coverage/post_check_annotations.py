#!/usr/bin/env python3
"""Attach uncovered-line annotations to a commit as a GitHub check run.

Takes the JSON written by `uncovered_in_diff.py --github-annotations` and
creates a completed `uncovered-diff-lines` check run on the given commit.
GitHub renders each annotation inline in the PR's Files changed tab — the
GitHub-native answer to "show me what this diff leaves uncovered", with no
coverage vendor involved.

The check never gates: its conclusion is `success` when every added line is
covered (or the diff has no coverable lines) and `neutral` otherwise. Making
it required would be a branch-protection decision, not a flag here.

Only GitHub Apps can create check runs, so this needs an Actions
`GITHUB_TOKEN` (the github-actions app); a user PAT cannot exercise it —
use `--dry-run` to inspect the payloads locally. In CI, an API failure
prints a workflow warning and exits 0: annotations are a visibility
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
import sys
import urllib.error
import urllib.request

# GitHub rejects requests carrying more than 50 annotations.
_CHUNK = 50
# A diff uncovered enough to exceed this needs a test-writing session, not a
# longer listing; the summary counts what the cap cut.
_CAP = 200


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


def _title(considered: int, uncovered: int, uninstrumented: int) -> "tuple[str, str]":
    """Return the check run's (title, conclusion) for the given counts."""
    if considered == 0:
        return "no coverable lines in this diff", "success"
    if uncovered == 0 and uninstrumented == 0:
        return "every added line is covered", "success"
    parts = []
    if uncovered:
        parts.append(f"{uncovered} uncovered added line{'s' if uncovered != 1 else ''}")
    if uninstrumented:
        parts.append(
            f"{uninstrumented} changed file{'s' if uninstrumented != 1 else ''} "
            "with no coverage record"
        )
    return ", ".join(parts), "neutral"


def _summary(truncated: int) -> str:
    lines = [
        "Computed from this run's combined `bazel coverage` report intersected "
        "with the PR diff — the same lines `codecov/patch` scores. Each "
        "annotation marks added lines no test executed; they render inline in "
        "the **Files changed** tab.",
        "",
        "This check is informational and never blocks a merge.",
        "",
        "Reproduce locally: download the `bazel-coverage-lcov` artifact and run "
        "`python3 tools/coverage/uncovered_in_diff.py <artifact-dir> --base "
        "origin/main` (see docs/skills/coverage.md).",
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
        description="Post uncovered-diff-line annotations as a GitHub check run."
    )
    parser.add_argument(
        "annotations",
        help="JSON produced by uncovered_in_diff.py --github-annotations",
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

    with open(args.annotations, "r", encoding="utf-8") as fh:
        data = json.load(fh)
    annotations = data["annotations"]
    truncated = max(0, len(annotations) - _CAP)
    annotations = annotations[:_CAP]

    title, conclusion = _title(
        data["considered_files"], data["uncovered_lines"], data["uninstrumented_files"]
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

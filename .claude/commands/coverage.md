---
allowed-tools:
  - Bash(gh:*)
  - Bash(git branch:*)
  - Bash(git diff:*)
  - Bash(git fetch:*)
  - Bash(curl:*)
  - Bash(python3 tools/coverage/*)
  - Bash(diff-cover:*)
  - Bash(pip install diff-cover:*)
  - Bash(bazel coverage:*)
  - Bash(bazel info:*)
  - Bash(awk:*)
---

Report which lines a change leaves uncovered, using the coverage CI already
produced. Answers *are the lines this change adds tested* — not the
whole-repo percentage.

## Context

Current branch: `!git branch --show-current`
PR for this branch: `!gh pr list --head "$(git branch --show-current)" --json number --jq '.[0].number'`
Latest coverage run: `!gh run list --workflow=bazel-coverage.yml --branch "$(git branch --show-current)" --limit 1 --json databaseId,conclusion,headSha --jq '.[0] | "\(.databaseId) \(.conclusion) \(.headSha[0:8])"'`
Arguments: $ARGUMENTS

## Steps

1. Read `docs/skills/coverage.md`. It has the four routes, the verified
   Coveralls endpoints, and the gotchas that make a red check a false alarm.
   For an open PR the fastest look is often the `uncovered-diff-lines`
   check the coverage job already posted — its annotations mark the
   uncovered added lines inline in the Files changed tab.
2. Pick the target: `$ARGUMENTS` if it names a branch or PR, otherwise the
   current branch (above).
3. Prefer the CI artifact — it is the only route with line numbers:

   ```
   gh run download <run> -n bazel-coverage-lcov -D /tmp/cov
   python3 tools/coverage/filter_lcov.py /tmp/cov/*.info --output /tmp/combined.info
   diff-cover /tmp/combined.info --compare-branch origin/main --show-uncovered
   ```

   `diff-cover` comes from pip (`pip install diff-cover`).

   Run `gh run download` from inside the repository. If the coverage run has
   not finished, is older than 90 days, or does not exist, fall back to the
   Coveralls API (skill doc §2) and say which route you used.
4. Report per file: the uncovered added lines, and separately any changed
   first-party `.rs` file with no coverage record at all. If nothing is
   uncovered, say so plainly — do not pad with whole-repo percentages the
   user did not ask for.
5. Uncovered lines mean **write the test**, and name which one. Never propose
   widening the coverage-exemption pattern (`IGNORED_RE` in
   `tools/coverage/filter_lcov.py`) for shipping code. Do not edit
   any file unless the user asks for the tests to be written.

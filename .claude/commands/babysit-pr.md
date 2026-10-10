---
allowed-tools:
  - Bash(gh:*)
  - Bash(git status:*)
  - Bash(git branch:*)
  - Bash(git switch:*)
  - Bash(git fetch:*)
  - Bash(git merge:*)
  - Bash(git add:*)
  - Bash(git commit:*)
  - Bash(git push:*)
  - Bash(git diff:*)
  - Bash(git log:*)
  - Bash(git rev-parse:*)
  - Bash(cargo fmt:*)
  - Bash(cargo build:*)
  - Bash(cargo test:*)
  - Bash(cargo clippy:*)
  - Bash(bazel build:*)
  - Bash(bazel test:*)
  - Workflow(adversarial-review)
---

Babysit a pull request to merge readiness: iterate with CI and
adversarial review rounds until CI is fully green, the latest round on
the current head is quiet — it confirmed no findings, or every finding
it confirmed was declined on the record — and every finding, inline or
outside the diff, has a recorded response.

## Context

Current branch: `!git branch --show-current`
PR for this branch: `!gh pr list --head "$(git branch --show-current)" --json number,title,url --jq '.[] | "#\(.number) \(.title) \(.url)"'`
Arguments: $ARGUMENTS

## Steps

1. Resolve the PR: `$ARGUMENTS` if it names one, otherwise the PR for the
   current branch (above). If neither exists, stop and say so. Make sure
   the checkout is at the PR head — review rounds read the working tree.
2. Read `docs/skills/babysitting-prs.md` and
   `docs/skills/adversarial-review.md`, then run the loop the first one
   defines — exit criteria, reply-per-thread rule, watcher, CI
   diagnosis — with review rounds as the second one defines them.
3. **Run a review round on every new head** with the Workflow tool:
   `name: "adversarial-review"`, `args: {pr: <n>}`. It runs in the
   background next to the CI watcher, and it reads the working tree:
   **change nothing in the checkout until it returns** — diagnose CI
   failures, but hold every edit, commit, merge and push. The round
   posts nothing itself: post its `review` payload as a PR review
   (adversarial-review.md §Recording a round), quiet rounds included —
   the marker in the body is what makes the head count as reviewed —
   and only while its `commit_id` is still the PR head. A `skipped` or
   `superseded` result needs no post; a result with `complete: false`
   is not a review of the head — fix the cause it names and re-run.
4. Triage every confirmed finding (adversarial-review.md §Triage
   guidance): fix it or decline it with evidence. Reply on every thread
   with the fix SHA or the reason; record *Outside the diff* findings'
   outcomes in one PR comment by ID. A round counts as quiet when it
   confirmed nothing, or when every finding it confirmed was declined
   with its response recorded; a round with any fixed finding is not
   quiet, and the fix push needs a fresh round.
5. Fixing anything means the full quality gate before pushing
   (AGENTS.md rule 4), the commit-author convention (rule 6), one push
   per round, and a `gh pr view <n> --json state` check that the PR is
   still open right before the push.
6. Between events, run the background CI watcher the skill doc mandates
   (§Pacing) rather than sleeping on assumed durations. For unattended
   babysitting, wrap this command in `/loop`.
7. If round 4 still confirms findings, stop and report to the owner.
   When the exit criteria hold, report merge readiness — checks, review
   rounds with per-round confirmed / declined / refuted counts, thread
   status — and stop. Never merge the PR yourself.

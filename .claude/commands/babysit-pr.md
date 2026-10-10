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

**Run this in a fresh session (or after `/clear`), not in the session
that wrote the change** — every step re-reads this session's whole
context (babysitting-prs.md §Where to run it). This session stays thin:
it launches rounds, posts them and asks the owner; a fresh
`pr-round-fixer` subagent does each round's fixes.

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
   `scriptPath: ".claude/workflows/adversarial-review.js"`,
   `args: {pr: <n>}` — by path, never by `name:`, which can serve a
   stale cached copy of the file within a session. It runs in the
   background next to the CI watcher, and it reads the working tree:
   **change nothing in the checkout until it returns** — diagnose CI
   failures, but hold every edit, commit, merge and push. The round
   posts nothing itself: post its `review` payload as a PR review
   (adversarial-review.md §Recording a round), quiet rounds included —
   the marker in the body is what makes the head count as reviewed —
   and only while its `commit_id` is still the PR head. A `skipped` or
   `superseded` result needs no post; a result with `complete: false`
   is not a review of the head — fix the cause it names and re-run.
4. **Hand the round's findings to a fresh fixer.** Once the round is
   posted and no round is running, spawn the Agent tool with
   `subagent_type: "pr-round-fixer"` — a new agent, never a fork — and a
   prompt naming the PR number, any CI failure to fix, and any owner
   decisions already made. It triages every finding without a recorded
   outcome (adversarial-review.md §Triage guidance), fixes or declines
   each with evidence, runs the quality gate (AGENTS.md rule 4), commits
   (rule 6), checks the PR is still open, pushes once, and replies on
   every thread. Wait for its summary; relay it to the owner, and ask
   them (`AskUserQuestion`) about any decision it returns, then pass the
   answer on — to the next fixer's prompt, or by `SendMessage` to the
   same one. A round counts as quiet when it confirmed nothing, or when
   every finding it confirmed was declined with its response recorded;
   a round with any fixed finding is not quiet, and the fix push needs a
   fresh round.
5. Don't fix, gate or commit in this session yourself: that is the
   context growth the fixer exists to avoid. Diagnosing a CI failure to
   brief the fixer is fine.
6. Between events, run the background CI watcher the skill doc mandates
   (§Pacing) rather than sleeping on assumed durations. For unattended
   babysitting, wrap this command in `/loop`.
7. If round 4 still confirms findings, stop and report to the owner.
   When the exit criteria hold, report merge readiness — checks, review
   rounds with per-round confirmed / declined / refuted counts, thread
   status — and stop. Never merge the PR yourself.

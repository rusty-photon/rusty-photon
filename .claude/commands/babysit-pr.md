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
  - Bash(cargo fmt:*)
  - Bash(cargo build:*)
  - Bash(cargo test:*)
  - Bash(cargo clippy:*)
  - Bash(bazel build:*)
  - Bash(bazel test:*)
---

Babysit a pull request to merge readiness: iterate with CI and Copilot
review until CI is fully green, the latest Copilot round is quiet — it
produced no findings, or every finding it produced was declined on the
record — and every review finding — inline or suppressed — has a
recorded response.

## Context

Current branch: `!git branch --show-current`
PR for this branch: `!gh pr list --head "$(git branch --show-current)" --json number,title,url --jq '.[] | "#\(.number) \(.title) \(.url)"'`
Arguments: $ARGUMENTS

## Steps

1. Resolve the PR: `$ARGUMENTS` if it names one, otherwise the PR for the
   current branch (above). If neither exists, stop and say so.
2. Read `docs/skills/babysitting-prs.md` and run its loop — it defines
   the exit criteria, the reply-per-thread rule, the exact `gh api`
   calls (Copilot re-request does not work via `gh pr edit`), and the
   triage guidance.
3. **Read the body of every Copilot review, not just
   `pulls/<n>/comments`.** Most findings arrive *suppressed* — inside a
   `<details><summary>Suppressed comments (n)</summary>` block in the
   review body — where they create no thread, do not appear in the
   comments endpoint, and do not stop the summary line from reading
   "generated no new comments". Evaluate each one exactly like an inline
   comment (same triage priors), fix or decline it, and record the
   outcome as a PR comment, since there is no thread to reply on. A
   round counts as quiet when it carries no findings at all, or when
   every finding it carried — inline or suppressed — was declined with
   its response recorded; a round with any fixed finding is not quiet,
   and the push needs a fresh round. §Suppressed comments in the skill doc has
   the `gh api .../reviews` + `jq` call — run it after every round, over
   every review since your last push.
4. Fixing anything means the full quality gate before pushing
   (AGENTS.md rule 4) and the commit-author convention (rule 6).
5. Between events, run the background watcher the skill doc mandates
   (§Pacing) — exit on new Copilot round / failed check / no checks
   pending — rather than sleeping on assumed durations. The watcher only
   detects that a round landed; reading it is step 3's job, so never
   treat a watcher exit as evidence the round was quiet. For unattended
   babysitting, wrap this command in `/loop`.
6. When the exit criteria hold, report merge readiness — checks, review
   rounds with per-round inline **and** suppressed counts, thread status
   — and stop. Never merge the PR yourself.

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
  - Skill(code-review)
---

Babysit a pull request to merge readiness: iterate with CI and code
review until CI is fully green, the latest review of the current head is
clean — it raised no findings, or every finding it raised was declined on
the record — and every finding has a recorded response.

## Context

Current branch: `!git branch --show-current`
PR for this branch: `!gh pr list --head "$(git branch --show-current)" --json number,title,url --jq '.[] | "#\(.number) \(.title) \(.url)"'`
Arguments: $ARGUMENTS

## Steps

1. Resolve the PR: `$ARGUMENTS` if it names one, otherwise the PR for the
   current branch (above). If neither exists, stop and say so. Make sure
   the checkout is at the PR head — the reviewer reads the working tree.
2. Read `docs/skills/babysitting-prs.md` and `docs/skills/code-review.md`,
   then run the loop the first one defines — exit criteria,
   reply-per-thread rule, watcher, CI diagnosis.
3. Review every new head with the Skill tool: `skill: "code-review"`,
   `args: "high <n> --comment --max-findings all"`. Then post the round comment
   (babysitting-prs.md, step 2 of the loop).
4. Triage every finding (code-review.md §Triage), fix or decline each with
   evidence, run the quality gate (AGENTS.md rule 4), commit (rule 6),
   check the PR is still open, push once per round, and reply on every
   thread.
5. Between events, run the background CI watcher the skill doc mandates
   (§Pacing) rather than sleeping on assumed durations. For unattended
   babysitting, wrap this command in `/loop`.
6. If the fourth review still raises findings you fix, stop and report to
   the owner. When the exit criteria hold, report merge readiness —
   checks, each review with its fixed / declined counts, thread status —
   and stop. Never merge the PR yourself.

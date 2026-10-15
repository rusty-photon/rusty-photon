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
  - Agent
---

Babysit a pull request to merge readiness: iterate with CI and code
review until CI is fully green, the latest review of the current head is
clean — it raised no findings, or every finding it raised was declined on
the record — and every finding has a recorded outcome.

## Context

Current branch: `!git branch --show-current`
PR for this branch: `!gh pr list --head "$(git branch --show-current)" --json number,title,url --jq '.[] | "#\(.number) \(.title) \(.url)"'`
Arguments: $ARGUMENTS

## Steps

1. Resolve the PR: `$ARGUMENTS` if it names one, otherwise the PR for the
   current branch (above). If neither exists, stop and say so. Make sure
   the checkout is at the PR head — the reviewer reads the working tree.
2. Read `docs/skills/babysitting-prs.md` and `docs/skills/code-review.md`,
   then run the loop the first one defines — exit criteria, rating step,
   round comment, watcher, CI diagnosis.
3. Review every new head with the Skill tool: `skill: "code-review"`,
   `args: "high <n> --max-findings all"`. No `--comment`: the findings
   come back here, not to the PR.
4. Rate the findings with one fresh `general-purpose` subagent — never a
   fork — using the prompt in code-review.md §The rating step.
5. Act on each verdict (babysitting-prs.md, step 4 of the loop): fix what
   rates fix, fix an optional one only if the round pushes anyway and the
   change is small and in code the PR touches, decline the rest once the
   code shows no trigger the rating
   missed, and overrule a verdict only with evidence.
   Run the quality gate (AGENTS.md rule 4), commit (rule 6), check the PR
   is still open, and push once per round. Then post the round comment
   (step 8) and reply in any human reviewer's thread.
6. Between events, run the background CI watcher the skill doc mandates
   (§Pacing) rather than sleeping on assumed durations. For unattended
   babysitting, wrap this command in `/loop`.
7. Fix nothing the fourth review raises without the owner's say: decline
   its optional findings, and if any rate fix, stop and report to the
   owner. When the exit criteria hold, report merge readiness —
   checks, each review with its fixed / declined counts, thread status —
   and stop. Never merge the PR yourself.

# Skill: Babysitting Pull Requests

## When to Read This

- After opening a pull request that must reach merge readiness
- When asked to "babysit" a PR
- When addressing review findings or human review comments on an open PR

Review rounds themselves — what runs, how findings are verified, how a
round is recorded, how to triage — are in
[adversarial-review.md](adversarial-review.md). Read it too.

## What "merge-ready" means

A babysat PR is done when **all** of these hold at the same time, on the
latest push:

1. **CI fully green** — every required check plus any path-triggered
   workflow the PR woke up (e.g. `msi.yml` on packaging changes). A slow
   leg still running means not done.
2. **A quiet review round on the head** — the newest adversarial-review
   round's marker names the current head SHA (`head=` is present only on
   a complete round, and only markers posted by someone with write
   access count — adversarial-review.md §Recording a round), and that
   round either confirmed no findings, or
   every finding it confirmed was **declined** with its reason recorded.
   A decline changes no code, so there is nothing for another round to
   review. Any finding that led to a fix voids the round: the fix push
   needs its own round. Any later push — docs included — needs one more
   round, with one exception: a merge of `origin/main` that resolved no
   conflicts adds nothing of the PR's own, so the earlier quiet round
   stands; say so in the report.
3. **Every review finding has a recorded response** — a reply on every
   thread (the rounds' inline findings and any human reviewer's), and a
   PR comment covering each *Outside the diff* finding by ID.
4. **No merge conflicts** (`gh pr view <n> --json mergeable`).

Then report merge readiness and stop. Merging is the repo owner's
decision and action — never merge the PR yourself, and all work stays
on the feature branch, never on `main` (rule 5).

## The loop

Start by classifying the PR — `gh pr view <n> --json
state,isDraft,author,mergeable,headRefOid` — and make sure the local
checkout is at its head (`git rev-parse HEAD`): review rounds read the
working tree. Then iterate:

1. **Start both waits after every push** (and once at the start): the
   CI watcher (§Pacing) and a review round (the Workflow tool with
   `scriptPath: ".claude/workflows/adversarial-review.js"`,
   `args: {pr: <n>}` — by path, not by name, so the checkout's file
   runs; adversarial-review.md §What a round is). Both run in the background; neither ends the
   other's wait. **While the round runs, change nothing in the
   checkout** — no edits, commits, merges or pushes: its reviewers are
   reading that working tree (adversarial-review.md §What a round is).
   Diagnose freely; hold the fixes.
2. **CI failure** → reproduce and diagnose; once no round is running,
   fix locally and run the full quality gate (rule 4) before the push.
   A CI failure that lands mid-round waits for the round, and its fix
   joins the round's fixes in one push.
3. **Merge conflict** → once no round is running, merge `origin/main`
   into the branch (don't rebase a branch that has review history),
   resolve, gate, push. Conflict resolution can also import upstream
   scope changes — re-read what landed on `main`, don't just take
   "ours".
4. **When the round lands, record it, then triage it.** Post its
   `review` payload as described in adversarial-review.md §Recording a
   round — a quiet round too, since its marker is the evidence for
   criterion 2 — but only while its `commit_id` is still the PR head. A
   round that returns `skipped` or `superseded` has nothing to post. A
   round with `complete: false` is not a review of the head: fix the
   cause it names (an uninstalled plugin, a checkout changed mid-round)
   and run it again. Then triage every finding honestly
   (adversarial-review.md §Triage guidance):
   - Legitimate (even partially) → fix it.
   - Wrong → decline **in the reply**, with evidence: a code pointer,
     doc link, or reproduction.
   - Never fix silently, never ignore. If the same wrong claim keeps
     recurring, consider making the code or docs unambiguous instead of
     re-litigating — often cheaper than another round.
5. **Push the fixes** — batched, one push per round (commit author per
   rule 6). Before pushing, confirm the PR is still open
   (`gh pr view <n> --json state`): with `delete_branch_on_merge` on, a
   push to a PR the owner has merged in the meantime silently recreates
   its deleted branch (`remote: Create a pull request … pull/new/…` in
   the push output is the tell).
6. **Reply on every thread** — what changed plus the commit SHA, or why
   declined — and post one PR comment with the outcome of each
   *Outside the diff* finding, by ID. The next round's scope stage reads
   these outcomes, so a declined finding is not raised again:

   ```sh
   gh api 'repos/{owner}/{repo}/pulls/<n>/comments/<comment-id>/replies' \
       -X POST -f body="Fixed in <sha> — <what changed>."
   ```

7. **Go to step 1 for the new head.** The round reviews the delta since
   the head the previous round reviewed.

Four rounds is the budget: if round 4 still confirms findings, stop and
report to the owner instead of starting round 5
(adversarial-review.md §Rounds and convergence).

Comments from human reviewers go through the same loop, except: when
inclined to decline, ask the reviewer rather than unilaterally closing
the discussion.

A bot-authored PR (dependabot, github-actions) needs a round only if you
changed code on it; then the round covers your change. A draft PR is
reviewed like any other.

## Pacing — watch, don't sleep

Babysitting MUST be event-driven: run a **background watcher** — a loop
that polls the PR cheaply (every ~60–90 s) and exits on the first
actionable event — then act on what it reports. Never sleep for a
guessed interval, and never assume a leg's duration from memory: when a
duration matters, measure it (`gh run list --workflow=<wf>.yml` shows
real run times).

The review round needs no watcher: it is a background workflow, and its
completion notifies you. The CI watcher exits on whichever comes first:
the PR is **no longer open**, it is **conflicting**, any **check
failed**, **no checks pending**, or a **new review from someone else**
(a human reviewer) beyond the baseline it started with. The shape:

```sh
# watch-pr.sh <pr-number> <others-review-baseline>
# ($1 must be the numeric PR id: the gh api call below cannot take a URL/branch)
# The baseline is the same count taken just before starting the watcher:
#   gh api --paginate 'repos/{owner}/{repo}/pulls/<n>/reviews' \
#     | jq -s --arg me "$(gh api user --jq .login)" '[.[][] | select(.user.login != $me)] | length'
# Bounded deliberately: an unbounded watch that has wedged looks exactly
# like one that is still waiting, so give it a deadline it can report.
me=$(gh api user --jq .login)
# An empty login would count your own reviews as someone else's.
[ -n "$me" ] || { echo "cannot resolve the gh user"; exit 1; }
for _ in $(seq 1 90); do   # ~90 min at the 60 s poll at the foot of the loop
  # A merged/closed PR never settles: without this the loop runs to its
  # deadline while looking perfectly healthy. Empty defaults to OPEN on
  # purpose, like the :-defaults below — a transient gh failure must not
  # read as "closed" and end the watch early. A persistent one still
  # surfaces, as the loop then hits the deadline and says so.
  state=$(gh pr view "$1" --json state --jq .state)
  [ "${state:-OPEN}" != "OPEN" ] && { echo "PR is $state"; exit 0; }
  # A CONFLICTING PR gets no pull_request runs at all — see "When no
  # checks appear at all".
  mergeable=$(gh pr view "$1" --json mergeable --jq .mergeable)
  [ "${mergeable:-UNKNOWN}" = "CONFLICTING" ] && { echo "PR is CONFLICTING: no CI will run — merge origin/main (step 3)"; exit 0; }
  # Every `--paginate` read slurps (`jq -s`): gh emits one array per page,
  # and `.[][]` reaches the reviews only once those arrays are gathered into
  # one. Your own reviews — the rounds you post, and the review object
  # GitHub creates for each thread reply — are excluded by login.
  others=$(gh api --paginate "repos/{owner}/{repo}/pulls/$1/reviews" \
    | jq -s --arg me "$me" '[.[][] | select(.user.login != $me)] | length')
  failed=$(gh pr checks "$1" --json bucket --jq '[.[] | select(.bucket == "fail")] | length')
  pending=$(gh pr checks "$1" --json bucket --jq '[.[] | select(.bucket == "pending")] | length')
  # `gh pr checks` exits non-zero with "no checks reported" when no run was
  # created; the :-default below keeps the loop alive through a transient
  # failure, and this counter stops it saying so instead of waiting out
  # the deadline when it persists.
  if [ -z "$pending" ]; then nochecks=$((${nochecks:-0} + 1)); else nochecks=0; fi
  [ "${nochecks:-0}" -ge 3 ] && { echo "no checks reported — see \"When no checks appear at all\""; exit 0; }
  # The :-defaults keep a transient gh/jq failure (empty variable) from
  # erroring the loop or reading as an exit condition: a failed query must
  # never count as "check failed", "nothing pending" or a new review.
  if [ "${failed:-0}" -gt 0 ]; then
    sleep 15  # a job re-run's attempt switch can transiently surface the prior attempt's fail
    failed=$(gh pr checks "$1" --json bucket --jq '[.[] | select(.bucket == "fail")] | length')
    [ "${failed:-0}" -gt 0 ] && { echo "check failed"; exit 0; }
  fi
  [ "${others:-0}" -gt "$2" ] && { echo "new review from someone else"; exit 0; }
  [ "${pending:-1}" -eq 0 ]   && { echo "no checks pending"; exit 0; }
  sleep 60
done
echo "watcher timed out"   # never silently: "nothing happened" is a result
```

(`--json bucket` is the machine contract — normalized
`pass`/`fail`/`pending`/`skipping`/`cancel` buckets; never parse the
human-formatted table.)

Run it via your harness's background-task facility (or `&` + `wait`) so
the wait costs nothing and reaction time is one poll interval.

Reference durations — for recognizing a stuck leg, never for sleeping:

- `bazel.yml` legs finish in ~4–10 minutes on a typical PR diff, on
  **all three platforms** — the remote cache limits work to the
  affected targets. Only a cold or invalidated cache, or a graph-wide
  change (a dep bump), pushes them past that.
- `windows-latest` **packaging** legs (`msi.yml`) are the true long
  pole at 40–90 minutes. That number applies to packaging workflows
  only — do not transfer it to the bazel test legs.

Every push restarts both waits, so don't push code that is about to
change again: batch the fixes for a round into one push, docs tweaks
included.

Three things a watcher must get right:

- **Check the PR is still open first.** A merged or closed PR never
  settles, and the loop spins to its timeout looking healthy.
- **Exclude your own reviews from the review count.** `pulls/<n>/reviews`
  carries a review object for every round you post *and* for each reply
  you post to a review comment; counting them fires phantom "new review"
  events right after you finish replying.
- **Settled CI does not end a wait for the round, and a finished round
  does not end a wait for CI.** They are separate criteria that can
  become true minutes apart; exiting on the first one and reporting
  readiness asserts something never checked.

### When no checks appear at all

`gh pr checks` saying *"no checks reported"* is not a slow queue — it
means no run was created. Check the cheap cause first: **a PR that is
`CONFLICTING` gets no `pull_request` runs at all**, because GitHub cannot
build the merge commit those runs check out — so the PR can look
reviewed but is untested. `gh pr view <n> --json mergeable` answers it;
merging `origin/main` into the branch (step 3 of the loop) makes the
next push run normally. On PR #1335 two pushes in a row went un-run this
way after `main` moved under the branch, with other PRs' runs landing
throughout. Only then look for an Actions-side cause: whether runs are
being created **repo-wide** (`gh run list --limit 20`) and whether
[githubstatus.com](https://www.githubstatus.com/api/v2/summary.json)
shows an Actions incident. During the 2026-08-06 Actions outage, pushes
produced no `bazel`/`check` runs at all, while other branches' jobs sat
`queued` for hours — nothing about any PR was wrong.

**GitHub does not replay missed `pull_request` triggers.** Once Actions
recovers, the runs will not appear on their own. If every affected
workflow uses a bare `pull_request:` trigger — no `types:` filter, so
the default `[opened, synchronize, reopened]` applies, which is the case
for `bazel.yml`, `check.yml` and `bazel-coverage.yml` — then closing and
reopening the PR re-fires them **without a push**, which keeps the head
unchanged and so preserves a quiet review round that an empty commit
would void. Verify the triggers first; a workflow that filters `types:`
may not include `reopened`.

One caveat before closing a PR: with `delete_branch_on_merge` enabled,
confirm the PR is not merged in the interim — a later `git push` to a
deleted branch silently recreates it, which looks like a resurrected
merged branch.

### When only the ConformU legs go red

`conformu.yml` pins **no** ConformU version: it calls
`ivonnyssen/conformu-install@v3`, whose `version` input defaults to
`latest` and is resolved against `ASCOMInitiative/ConformU/releases/latest`
on every run. That is deliberate — ConformU is the spec validator, and
drifting behind it is the worse failure — but it means **a release upstream
can turn a green nightly red with no commit of ours**.

So when the conformu legs are the only thing red, check for a new release
before reading the diff:

```sh
gh api repos/ASCOMInitiative/ConformU/releases \
  --jq '.[0:3][] | "\(.tag_name | ltrimstr("v"))\t\(.published_at)"'
```

(Tags are `v`-prefixed; `ltrimstr` drops it so the output compares directly
against the version ConformU itself prints, below.)

If one landed between the last green run and the red one, that is the
first hypothesis. Confirm what a run actually installed rather than
inferring it from timestamps — the version is stamped in the job log:

```sh
gh run view <run-id> --log | grep -oiE "conform universal [0-9.]+"
```

The same drift runs the other way for hardware validation, where the
version gate is owned by
[hardware-validation.md](hardware-validation.md).

### When a BDD suite goes red

The `bazel test (BDD)` step runs with `--test_output=all`, so its job log
is tens of thousands of lines, and tail-limited reads of it (the GitHub MCP
`get_job_logs` returns only the last 5,000 lines) miss most of it. A suite that
failed or timed out before the others finished has its failing scenario
above that window. Don't guess from the tail: when that step fails, the
job uploads every suite's own `test.log` / `test.xml` (per shard) as the
`bdd-testlogs-<os>-attempt-<n>` artifact. Download it and read the failing
target's log (`services/<svc>/bdd/[shard_N_of_M/]test.log`) directly.

A `TIMEOUT` with no failed step means a scenario stopped making progress.
The last scenario printed is the one that stalled: cucumber's output is
normalised, so every scenario before it has already finished
([testing.md §5.7](testing.md#57-never-block-in-a-step--the-whole-suite-shares-one-poll-loop)
explains why one blocked step can freeze all of them).

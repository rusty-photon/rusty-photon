# Skill: Babysitting Pull Requests

## When to Read This

- After opening a pull request that must reach merge readiness
- When asked to "babysit" a PR
- When addressing Copilot (or human) review comments on an open PR

## What "merge-ready" means

A babysat PR is done when **all** of these hold at the same time, on the
latest push:

1. **CI fully green** — every required check plus any path-triggered
   workflow the PR woke up (e.g. `msi.yml` on packaging changes). A slow
   leg still running means not done.
2. **A quiet Copilot round** — the most recent round either
   produced zero new comments **and no suppressed-comments section**
   (see *Suppressed comments* — the round summary says "generated no
   new comments" even when it carries findings), or produced findings
   that were **all declined**, each with its response recorded (thread
   reply, or PR comment for suppressed findings). A decline changes no
   code, so there is nothing for a follow-up round to re-review;
   re-requesting one anyway just costs a redundant round — this is the
   one state in which a manual request is not a no-op (see *The loop*).
   Any finding
   that led to a fix voids the round, and a quiet round only counts if
   nothing was pushed after it; any later push (docs included) needs
   one more round, which the push itself requests (see *The loop*). A
   Copilot review whose body says it was **unable to review** — the
   quota notice, or the "encountered an error" notice — is not a round:
   it reviewed nothing, so it satisfies none of this, and it still
   raises the round count (step 7).
3. **Every review thread has a reply** (see the loop below) — and note
   that thread coverage alone does not satisfy criterion 2, because
   suppressed comments create no thread to cover.
4. **No merge conflicts** (`gh pr view <n> --json mergeable`).

Then report merge readiness and stop. Merging is the repo owner's
decision and action — never merge the PR yourself, and all work stays
on the feature branch, never on `main` (rule 5).

## The loop

**Don't request a Copilot round by hand on a PR a human opened** —
the fallback cases are in step 7 and nothing else qualifies. The
*Co-Pilot Code Reviews* ruleset (rule type `copilot_code_review`,
`review_on_push: true`, drafts excluded, active on every branch —
`gh api 'repos/{owner}/{repo}/rules/branches/main'` shows it) requests
a round when a non-draft PR opens, when a draft is marked ready (#900),
and on every push. The PR's timeline shows it acting: a
`review_requested` event 1–5 s after the push and a
`copilot_work_started` event 30–95 s later —

```sh
gh api --paginate 'repos/{owner}/{repo}/issues/<n>/timeline' \
  | jq -r '.[] | select((.event == "review_requested" and .requested_reviewer.login == "Copilot")
                      or .event == "copilot_work_started"
                      or (.event == "reviewed" and .user.login == "Copilot"))
           | "\(.created_at // .submitted_at)\t\(.event)\t\(.commit_id[0:8] // "")"' \
  | tail -5
```

A manual request on top of the ruleset's is a no-op only while that
request is pending: on PR #1387 nine POSTs, each seconds after a push,
added neither an event nor a round (18 events for 18 opens-or-pushes).
Once the round has landed, Copilot is no longer a requested reviewer,
and a POST then draws a redundant round on unchanged code (#1364: a
POST 14 s after a round drew a second review of the same SHA; #1314,
#1352, #1277, #1279 likewise). Either way it buys nothing.

The ruleset acts on nothing a bot opened: 17 dependabot and
github-actions PRs since #1263 drew zero requests across their opens,
pushes and rebases. On a bot-authored PR the manual request in step 7
is the only way to get a round — after the open and after every push
you make. A reopen draws no round (#715), which is what makes the
close-and-reopen trick under *When no checks appear at all* safe.

Start by classifying the PR — `gh pr view <n> --json isDraft,author,mergeable`
— then iterate:

1. **Watch** CI (`gh pr checks <n>`), mergeability, and new review
   comments (`gh api 'repos/{owner}/{repo}/pulls/<n>/comments'` — gh
   fills the `{owner}`/`{repo}` placeholders from the current repo).
2. **CI failure** → reproduce and fix locally; run the full quality gate
   (rule 4) before every push.
3. **Merge conflict** → merge `origin/main` into the branch (don't
   rebase a branch that has review history), resolve, gate, push.
   Conflict resolution can also import upstream scope changes — re-read
   what landed on `main`, don't just take "ours".
4. **Triage every new comment honestly:**
   - Legitimate (even partially) → fix it.
   - Factually wrong → decline **in the reply**, with evidence: a code
     pointer, doc link, or reproduction. Wrong comments still get
     replies.
   - Never fix silently, never ignore. If the same wrong claim keeps
     recurring, consider making the code or docs unambiguous instead of
     re-litigating — often cheaper than another round.
5. **Push the fixes** (commit author per rule 6).
6. **Reply on every thread** — what changed plus the commit SHA, or why
   declined — before the round the push drew lands. It starts at the
   push, and the SHA is known at commit time, so the replies can go up
   before or right after pushing:

   ```sh
   gh api 'repos/{owner}/{repo}/pulls/<n>/comments/<comment-id>/replies' \
       -X POST -f body="Fixed in <sha> — <what changed>."
   ```

7. **Wait for the round the push drew, and read it before calling it
   quiet.** The ruleset requested it the moment the push landed; the
   watcher below exits when the Copilot round count rises. Then read
   the review's body first: one that says Copilot was *unable to
   review* — the quota notice, or "encountered an error … re-requesting
   a review" — is not a round. It reviewed nothing and satisfies no
   part of exit criterion 2. Quota is charged to the PR's author, so no
   POST of yours changes it (#1363: a re-request four minutes later
   drew the same notice in 6 s); stop and report it to the owner. The
   error notice itself asks for a re-request, and that is a legitimate
   manual request (#1267). A round whose findings were all declined on
   the record needs no follow-up: nothing was pushed, so nothing drew
   one, and a POST now would draw a redundant round on unchanged code.

   **Don't push while a round is in flight.** The push's request is
   deferred until that round lands, the round that lands first is
   stamped with the pre-push SHA, and the push's own round comes 3–6
   minutes after it (nine cases across #902, #1308, #1309, #1314 and
   #1335). Read the timeline tail before pushing — a newest Copilot
   event of `review_requested` or `copilot_work_started` with no
   `reviewed` after it means a round is in flight — and batch the fixes
   instead. If you pushed anyway, expect two rounds and triage only the
   second.

   **The fallback request is for a request that never appeared, not
   for a slow round.** About a minute after the push (or after an
   in-flight round lands), read the timeline: a `review_requested` →
   Copilot event at or after your push means the ruleset acted — wait,
   however long (median 4 minutes, 90 % within 8, up to 17 minutes
   seen). None means it did not: request by hand, which is also the
   only way on a bot-authored PR.

   ```sh
   gh api 'repos/{owner}/{repo}/pulls/<n>/requested_reviewers' \
       -X POST -f 'reviewers[]=copilot-pull-request-reviewer[bot]'
   ```

   (`gh pr edit --add-reviewer Copilot` does **not** work; use the API
   call above.)

   **An empty `reviewRequests` afterwards does not mean the request
   failed.** A 2xx is the confirmation. `gh pr view <n> --json
   reviewRequests` never lists a Bot reviewer — gh's exporter handles
   only User and Team entries — so `[]` says nothing about the request.
   Read the REST endpoint `pulls/<n>/requested_reviewers`, or better the
   timeline's `copilot_work_started` event, then wait for the round
   count to rise. Re-POSTing on the strength of the empty list changes
   nothing while the request is pending and draws a redundant round
   once it is not.

Repeat until the exit criteria hold. Comments from human reviewers go
through the same loop, except: when inclined to decline, ask the
reviewer rather than unilaterally closing the discussion.

## Suppressed comments — the findings that create no thread

Copilot hides some findings inside a `<details><summary>Suppressed
comments (n)</summary>` block in the **review body**. They are ordinary
review findings, often the sharpest ones, but they:

- do **not** appear in `pulls/<n>/comments`,
- do **not** create a review thread, so there is nothing to resolve or
  reply to, and
- do **not** stop the summary line from reading *"generated no new
  comments"*.

A loop that checks only inline comments and thread replies therefore
satisfies both criteria while shipping every one of them. On PR #902
this was not marginal: **17 suppressed findings across the rounds
against 4 inline comments**, and the suppressed set included a real
host-endianness bug, a test that could pass vacuously, a validation
record asserting evidence it did not contain, and a factually wrong code
comment.

#902 is not an outlier; suppressed is where the findings normally land.
PR #923 ran five rounds carrying **6 suppressed findings against 1
inline comment**, and rounds 2–4 were suppressed-*only* — a loop keyed
on threads would have seen "generated no new comments", found its single
thread already replied, and declared merge readiness three rounds early.
Two of the six were substantive: a constructor that silently disabled a
validation rule, and one alignment constant spelled two different ways,
which would have produced exactly the ConformU failure the code exists
to prevent. Neither is reachable by any local gate — clippy, Bazel and
the tests were green for all five rounds.

Expect the round *after* a suppressed-comment fix to find fallout from
that fix. On #923 rounds 3 and 4 flagged doc drift and a redundant shim
that the previous round's own fixes introduced. That is convergence
working, not review thrashing — but it does mean a small fix never
justifies calling the PR ready before the round its push drew has
landed and been read.

So parse the body of every round:

```sh
# jq -s '.[][]' for the same reason as the watcher in §Pacing: --paginate
# concatenates one array per page, which a bare '.[]' mishandles.
#
# Print each matching body whole. Do NOT pipe through `grep -A<n>`: that
# caps the output at n lines per match, so a long or repeated suppressed
# section is silently cut off — which is exactly the failure this section
# exists to prevent. (On #902 a `grep -A100` form dropped 17 lines.)
gh api --paginate 'repos/{owner}/{repo}/pulls/<n>/reviews' \
  | jq -s -r '.[][]
      | select(.user.login == "copilot-pull-request-reviewer[bot]")
      | select(.body | test("Suppressed comments"))
      | "=== \(.commit_id[0:8])\n\(.body)"'
```

Triage them exactly like inline comments (same priors below). Since
there is no thread, record the outcome as a PR comment instead, so the
reasoning is on the record. A suppressed finding declined this way
counts toward criterion 2 exactly like a declined inline comment: a
round whose findings were all declined is quiet.

**Read every review since your last push, not just the newest.** A round
can arrive as *two* review objects seconds apart, and taking only the
last silently drops the other's findings. Worse, the `commit_id` on a
review is not reliably the head SHA — on #902 the review carrying the
findings was recorded against the *previous* commit while reviewing the
head's content, so a watcher keyed on `commit_id == HEAD` matched the
empty one and reported a quiet round that was not quiet.

## Pacing — watch, don't sleep

Babysitting MUST be event-driven: run a **background watcher** — a loop
that polls the PR cheaply (every ~60–90 s) and exits on the first
actionable event — then act on what it reports. Never sleep for a
guessed interval, and never assume a leg's duration from memory: when a
duration matters, measure it (`gh run list --workflow=<wf>.yml` shows
real run times).

A watcher exits on whichever comes first: a **new Copilot review**
beyond the round count it started with, any **check failed**, or **no
checks pending**. The shape:

```sh
# watch-pr.sh <pr-number> <copilot-round-baseline>
# ($1 must be the numeric PR id: the gh api call below cannot take a URL/branch)
# Bounded deliberately: an unbounded watch that has wedged looks exactly
# like one that is still waiting, so give it a deadline it can report.
for _ in $(seq 1 90); do   # ~90 min at the 60 s poll at the foot of the loop
  # A merged/closed PR never settles: without this the loop runs to its
  # deadline while looking perfectly healthy. Empty defaults to OPEN on
  # purpose, like the :-defaults below — a transient gh failure must not
  # read as "closed" and end the watch early. A persistent one still
  # surfaces, as the loop then hits the deadline and says so.
  state=$(gh pr view "$1" --json state --jq .state)
  [ "${state:-OPEN}" != "OPEN" ] && { echo "PR is $state"; exit 0; }
  # A CONFLICTING PR gets no pull_request runs at all but still draws
  # Copilot rounds (the ruleset reviews the head), so a quiet round on one
  # proves nothing about CI — see "When no checks appear at all".
  mergeable=$(gh pr view "$1" --json mergeable --jq .mergeable)
  [ "${mergeable:-UNKNOWN}" = "CONFLICTING" ] && { echo "PR is CONFLICTING: no CI will run — merge origin/main (step 3)"; exit 0; }
  rounds=$(gh api --paginate "repos/{owner}/{repo}/pulls/$1/reviews" \
    | jq -s '[.[][] | select(.user.login == "copilot-pull-request-reviewer[bot]")] | length')
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
  # never count as "no rounds", "check failed", or "nothing pending".
  if [ "${failed:-0}" -gt 0 ]; then
    sleep 15  # a job re-run's attempt switch can transiently surface the prior attempt's fail
    failed=$(gh pr checks "$1" --json bucket --jq '[.[] | select(.bucket == "fail")] | length')
    [ "${failed:-0}" -gt 0 ] && { echo "check failed"; exit 0; }
  fi
  # Print the newest review's opening so a quota or error notice — a review
  # object that reviewed nothing — is seen for what it is (step 7).
  [ "${rounds:-0}" -gt "$2" ]  && { echo "new Copilot round:"; gh api --paginate "repos/{owner}/{repo}/pulls/$1/reviews" \
    | jq -r '[.[][] | select(.user.login == "copilot-pull-request-reviewer[bot]")] | last | .body[0:160]'; exit 0; }
  [ "${pending:-1}" -eq 0 ]    && { echo "no checks pending"; exit 0; }
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

- Copilot rounds land a median 4 minutes after the push's request, 90 %
  within 8, with a tail to ~17 minutes (241 rounds, #1263–#1393). The
  `review_requested` event lands 1–5 s after the push and
  `copilot_work_started` 30–95 s later; a push's request made while a
  round is in flight is deferred until that round lands.
- `bazel.yml` legs finish in ~4–10 minutes on a typical PR diff, on
  **all three platforms** — the remote cache limits work to the
  affected targets. Only a cold or invalidated cache, or a graph-wide
  change (a dep bump), pushes them past that.
- `windows-latest` **packaging** legs (`msi.yml`) are the true long
  pole at 40–90 minutes. That number applies to packaging workflows
  only — do not transfer it to the bazel test legs.

Every push to a human-authored PR draws a round, so don't push code
that is about to change again, and never push while a round is in
flight (step 7): batch the fixes for a round into one push, docs tweaks
included. A draft PR draws no round (the ruleset excludes drafts);
marking it ready draws one (#900), and the step-7 timeline check tells
you whether it did.

Four things a watcher must get right — the first three each produced a
wrong answer on #902:

- **Check the PR is still open first.** A merged or closed PR never
  settles, and the loop spins to its timeout looking healthy.
- **Don't key "a new round" on `commit_id == HEAD`.** See *Suppressed
  comments*: reviews are not reliably stamped with the head SHA. Take a
  baseline count of Copilot reviews before the push and watch for it to
  rise, as the snippet above does. Then read *every* review past the
  baseline, not just the newest — one round can arrive as two objects.
- **Count Copilot's reviews, not the endpoint's length.** The
  `select(.user.login == "copilot-pull-request-reviewer[bot]")` filter
  above is load-bearing, not tidiness: `pulls/<n>/reviews` also carries a
  review object for **each reply you post to a review comment**. On #927
  a bare `length` read 3 rounds where Copilot had submitted 1 — your own
  two replies inflated it. That fires phantom "new round" events, and
  since the inflation arrives right when you finish replying, it can make
  a *stale* round look like the fresh one and end the loop early.
- **Settled CI does not end a wait for review, and a quiet round does
  not end a wait for CI.** They are separate criteria that can become
  true minutes apart; exiting on the first one and reporting readiness
  asserts something never checked.

### When no checks appear at all

`gh pr checks` saying *"no checks reported"* is not a slow queue — it
means no run was created. Check the cheap cause first: **a PR that is
`CONFLICTING` gets no `pull_request` runs at all**, because GitHub cannot
build the merge commit those runs check out — while Copilot's own run,
which works on the head commit, still appears, and the ruleset keeps
drawing rounds on it (#1335) — so the PR looks reviewed but untested. `gh pr view <n> --json mergeable` answers it; merging
`origin/main` into the branch (step 3 of the loop) makes the next push
run normally. On PR #1335 two pushes in a row went un-run this way
after `main` moved under the branch, with other PRs' runs landing
throughout. Only then look for an Actions-side cause: whether runs are
being created **repo-wide** (`gh run list --limit 20`) and whether
[githubstatus.com](https://www.githubstatus.com/api/v2/summary.json)
shows an Actions incident. During the 2026-08-06 Actions outage, pushes
produced Copilot runs but no `bazel`/`check` runs at all, while other
branches' jobs sat `queued` for hours — nothing about any PR was wrong.

**GitHub does not replay missed `pull_request` triggers.** Once Actions
recovers, the runs will not appear on their own. If every affected
workflow uses a bare `pull_request:` trigger — no `types:` filter, so
the default `[opened, synchronize, reopened]` applies, which is the case
for `bazel.yml`, `check.yml` and `bazel-coverage.yml` — then closing and
reopening the PR re-fires them **without a push**, which preserves an
already-earned quiet Copilot round that an empty commit would invalidate
(a reopen draws no Copilot round — #715 — so the earned round stands).
Verify the triggers first; a workflow that filters `types:` may not
include `reopened`.

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

## Triage guidance

Copilot is often right about edge cases (silent fall-throughs, masked
errors, hard-coded values that will drift) and often wrong about
repo-specific facts (labels, conventions, what other files already do).
Verify every claim against the code before acting on it — in both
directions: don't dismiss a real bug because the comment reads pedantic,
and don't "fix" working code because the comment sounds confident.

### What the record shows

Classifying all 1492 Copilot review threads across PRs #142–#808 (230
PRs that drew comments) gives a per-category prior worth triaging by.
Share of a category's comments that led to a real improvement:

| Category                        |   n | useful | harmful |
| ------------------------------- | --: | -----: | ------: |
| Races, locking, task lifetime    |  51 |    86% |      0% |
| Security                         |  57 |    83% |      2% |
| Validation / missing mirror site | 149 |    80% |      2% |
| Logic bugs                       | 213 |    78% |      9% |
| Error handling                   | 133 |    72% |      3% |
| Test quality                     | 161 |    58% |      6% |
| Doc / comment drift              | 562 |     9% |      3% |
| Style nits                       |  75 |     3% |      3% |
| "This won't compile"             |  32 |     0% |     84% |

(Performance and uncategorized comments, 59 threads, are omitted.)

One limit of this corpus: it was built from review *threads*, so it
contains no suppressed comments at all. The priors describe the inline
population only, and nothing here licenses discounting a suppressed
finding — on the two PRs where the split was measured, suppressed
findings were both the majority and, on average, the sharper half.

Read the first five rows closely — that is where review has caught
defects nothing else could. Doc drift is 38% of all comment volume and
the least productive; the useful minority is the subset where following
the text would cause a wrong action (bad units, a contradicted
contract, a recovery procedure missing a step).

Never act on a compile, borrow, lint or format claim: the Bazel and
clippy gates settled those before the review ran, and 27 of 32 such
claims were flatly wrong. The same applies to any confident assertion
about external tool behavior (udev precedence, systemd unit resolution,
rootless podman, Actions contexts, `shasum` flags) — verify against the
tool's manual before believing it.

Roughly one comment in eight is a duplicate: the same finding on
sibling files, or re-raised in a later round against a commit that
already fixed it. Check whether an intervening SHA addressed it before
writing a reply.

A suggested fix can be right about the defect and wrong about the
remedy, so verify the remedy too. On #902 a comment correctly called a
schema assertion too weak, then prescribed asserting on the definition's
`enum` array — but `schemars` renders a *documented* fieldless enum as
`oneOf[].const`, so implementing that literally would have asserted
against a key that does not exist. Dumping the actual artefact before
writing the assertion cost one command and caught it.

### Steering Copilot

`.github/copilot-instructions.md` and the path-scoped files in
`.github/instructions/` encode the above as review instructions. Copilot
code review reads **only the first 4000 characters** of each file, so
keep them short; the budget is per file, which is why the guidance is
split by `applyTo` path. Since July 2026 these are read from the PR's
head branch, so changes can be tested on a feature branch before merge.

Overflow is silent — the tail is simply never read, and nothing warns
you. Check after editing:

```sh
for f in .github/copilot-instructions.md .github/instructions/*.md; do
  printf '%5d  %s\n' "$(wc -c < "$f")" "$f"
done
```

`wc -c` counts **bytes**, and the limit is stated in characters — these
files carry em-dashes and curly quotes, so bytes run a little ahead
(`copilot-instructions.md`: 3983 bytes, 3961 characters). That is
deliberate: bytes ≥ characters, so passing the byte check guarantees
passing the character limit, and unlike `wc -m` it does not depend on
the caller's locale (`wc -m` silently counts bytes under `LC_ALL=C`).
It costs a few characters of headroom and never gives a false pass.

`docs/plans/**` has its own file: a plan records decisions and phasing,
and reviewing its prose as though it were the implementation it
describes produces detail findings that only an implementation can
settle.

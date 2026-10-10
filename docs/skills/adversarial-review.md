# Skill: Adversarial PR Review

## When to Read This

- Running a review round on a pull request — while babysitting it
  ([babysitting-prs.md](babysitting-prs.md)) or on its own
- Triaging the findings a round posted
- Tuning a reviewer lens or the verifier

## What a round is

A round is one pass of independent reviewers over a PR's head commit,
followed by an adversarial pass that tries to kill every finding before
anyone acts on it. It replaces the Copilot rounds this repo used until
2026-10: the reviewers are local agents, so a round runs when the
babysitter starts one rather than on every push, and nothing about it
is hidden in a review body.

A round has five stages:

1. **Scope** — the PR's purpose, its merge-base and head, the changed
   files, and — from the marker left by the previous round — the delta
   since the last reviewed head.
2. **Review** — the lenses below, picked by the paths the round covers,
   run in parallel. Each starts from a fresh context: never fork the
   authoring session into a reviewer. Independence from the author's
   reasoning is the point of the exercise.
3. **Dedupe** — findings that name the same defect from two lenses are
   merged before anything is spent verifying them twice.
4. **Verify** — each finding goes to skeptics prompted to refute it. A
   `high` finding gets three, each from a different angle (trace the
   trigger, check whether head already handles it, check every factual
   claim and the remedy), and survives only if at least two fail to
   refute it. Every other finding gets one skeptic, who must fail.
5. **Record** — the survivors are anchored to diff lines and returned
   as a ready-to-post GitHub review.

In Claude Code the whole round is the saved workflow
[`.claude/workflows/adversarial-review.js`](../../.claude/workflows/adversarial-review.js).
Run it from a checkout of the PR's head commit (it refuses otherwise,
because the reviewers read files from the working tree):

```text
/adversarial-review 1458
```

or, from an agent, the Workflow tool with `name: "adversarial-review"`
and `args: {pr: 1458}`. Optional args: `full: true` reviews the whole
PR again instead of the delta (after a redesign); `round` / `since`
override what the scope stage reads from the PR's previous rounds; and
`skip: ["silent-failures"]` leaves a lens out on purpose — the body
records it and the round still counts as complete, so use it only when
the owner has agreed (say, on a machine that cannot install the
plugin).

The workflow posts nothing. It returns the round's findings plus a
`review` payload for the caller to post (§Recording a round).

Operators without Claude Code run the same process by hand: one
reviewer per applicable lens, each given its agent file below as
instructions plus §Ground rules, then a separate skeptic per finding
following [`review-verifier.md`](../../.claude/agents/review-verifier.md).

## The lenses

Each lens hunts one class of defect, chosen from the record of what
review has actually caught here (§What the record shows). A lens runs
only when the round's files include a path it covers; lockfiles
(`Cargo.lock`, `MODULE.bazel.lock`) trigger nothing.

| Lens                | Agent                                                          | Hunts                                                                                     | Runs when the round touches                                              |
| ------------------- | -------------------------------------------------------------- | ----------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| concurrency         | [`review-concurrency`](../../.claude/agents/review-concurrency.md)   | races, guards across `.await`, task lifetime, error-path rollback, missing timeouts       | code (`.rs`, `.sh`, `.py`, `.ps1`, `.js`, `.ts`)                         |
| correctness         | [`review-correctness`](../../.claude/agents/review-correctness.md)   | logic bugs, silent wrongness, units and casts, config/serde, the other site needing the change | code, Cargo/Bazel manifests, config files                            |
| safety              | [`review-safety`](../../.claude/agents/review-safety.md)             | tenet 3 (no actuation on connect), secrets, injection, escaping, internal IPs             | code, workflows, installer and packaging files                           |
| tests               | [`review-tests`](../../.claude/agents/review-tests.md)               | tests that cannot fail, degenerate fixtures, stale evidence, leaks between tests          | `.rs`, `.feature`                                                        |
| silent-failures     | `pr-review-toolkit:silent-failure-hunter` (plugin)             | swallowed errors and unjustified fallbacks                                                | code                                                                     |
| ci-packaging        | [`review-ci-packaging`](../../.claude/agents/review-ci-packaging.md) | swallowed failures, publish ordering, incomplete wiring, pinning in CI/scripts/packaging  | `.github/workflows`, `.github/actions`, `scripts/`, `tools/`, `installer/`, `pkg/`, Bazel files, `Cargo.toml` |
| docs                | [`review-docs`](../../.claude/agents/review-docs.md)                 | text that would cause a wrong action; design docs a behaviour change left stale (rule 2); plans reviewed as plans | `.md`, or Rust under `services/`                         |

The silent-failures lens is Anthropic's `pr-review-toolkit` plugin,
enabled for this project in `.claude/settings.json`. Enabling it there
does not install it: run
`claude plugin install pr-review-toolkit@claude-plugins-official --scope project`
once per machine. Without it the workflow logs that the lens was
skipped and runs the rest — read that log line, because a round missing
a lens has covered less than it says. The plugin's other five agents
are denied in the same settings file: their descriptions invite
proactive use, and comment and style review are the categories the
record rates lowest.

## Ground rules

Every reviewer and skeptic follows these. The lens agents point here
rather than repeating them.

**Anchor every finding to what the PR is for.** Work out the change it
makes and the problem it solves, and review that. A finding must bear
on whether the PR achieves its purpose, or breaks something on the way.
The lens priorities say which defects are worth reporting *within* that
scope; they are not a licence to audit the surrounding system or to ask
for work the PR defers. A defect in code the PR neither changed nor
depends on is reported as pre-existing, not as a finding against it.

**When the diff is a document, review the document** — false claims,
internal contradictions, conflicts with the repo's decisions — not the
code it describes as though that code were written.

**Never predict a build, lint or format outcome.** Every PR has passed
or will pass Bazel on Linux and Windows, both clippy passes with
`-D warnings`, `cargo fmt` and the BDD suites before merge; those gates
settle it. Such claims were 84 % harmful in the record.

**Open the file before claiming anything about it.** You have the whole
checkout, not just a diff: never assert that a type, function, file or
config key does or does not exist, or behaves some way, without reading
it.

**Cite external behaviour or do not raise it.** Claims about udev
precedence, systemd resolution, GitHub Actions contexts, shell
redirection, tool flags and third-party crate semantics were the most
expensive wrong comments here. Quote the manual or the crate source.

**Never propose a sleep, retry or readiness loop** to make code or a
test tolerate a race — this repo rejects those as masking defects.
Report the race. Never propose a fix that only narrows a window; if the
correct fix is structural, say so.

**One finding per defect.** Raise it once, on the clearest instance,
and name the sibling sites in it.

**State the consequence concretely:** the input or state that triggers
it, and what goes wrong. If you are not confident it is real, report
nothing. A round that returns no findings is a good outcome, not a
failure to look hard enough.

**Severity.** `high`: can lose an imaging night, actuate or damage
hardware, leak a secret, corrupt data or a published artifact, or wedge
a service until restart. `medium`: wrong behaviour in a reachable but
narrower case, or a test that cannot fail. `low`: anything else worth a
maintainer's time. Style, phrasing and naming are not findings.

**Leave the checkout exactly as you found it.** Reviewers share one
working tree and run concurrently: no edits, no `git checkout`,
`stash`, `reset` or `switch`, no mutation tests. Read-only commands and
running existing tests are fine. A finding that needs a mutation test to
prove is reported with the test to run; the fixer runs it.

## Rounds and convergence

Round 1 reviews the whole PR. Every later round reviews the **delta**
since the head the previous round reviewed, read from the marker that
round left on the PR, and reports only defects the delta introduced or
prior findings it claims to fix but does not. The previous rounds'
findings and outcomes are handed to every lens: a declined finding is
not raised again without new evidence that the decline was wrong.

- A head that a round already reviewed is not reviewed again — the
  workflow returns `skipped`. Re-reviewing unchanged code buys nothing
  and costs a round.
- Expect the round after a fix to find fallout from that fix. That is
  convergence working, not churn — but it means a small fix never
  justifies calling a PR ready before its own round has run.
- Pass `full: true` when a push restructured the change enough that a
  delta no longer describes it.
- **Four rounds is the budget.** If round 4 still confirms findings,
  stop and report to the owner rather than starting round 5: findings
  that survive four rounds usually mean a design question that fixes
  cannot settle.

## Recording a round on the PR

The workflow's `review` field is a complete GitHub review payload (event
`COMMENT`, `commit_id` = the reviewed head). Post it:

```sh
# Write the workflow's `review` object to a file first; --input reads JSON.
gh api 'repos/{owner}/{repo}/pulls/<n>/reviews' -X POST --input review.json
```

- It posts under your account. That is expected: GitHub allows a
  `COMMENT` review on your own PR, and every finding carries its round
  ID (`R2.3`) and lens so it reads as the round's, not yours.
- Findings anchored to a diff line become inline threads. Reply on each
  as for any review thread (babysitting step 6).
- Findings outside the diff — a sibling site the PR missed is outside
  it by definition — sit in the review body under *Outside the diff*.
  Record their outcomes in one PR comment naming each ID.
- A quiet round is posted too. Its body and its marker are the evidence
  that the head was reviewed, which the merge-ready report cites.
- The body also lists, collapsed, what the verifier refuted and what it
  judged pre-existing. Neither is a finding. The refuted list lets a
  human audit the verifier; the pre-existing list is a source of
  follow-up issues, not of scope for this PR.
- The body carries a hidden marker,
  `<!-- adversarial-review round=<n> head=<sha> -->`. The next round's
  scope stage reads it; do not edit it out.

If the POST fails with 422 because a line no longer resolves (a push
landed between the round and the post), the round reviewed a stale
head: run a new round instead of patching the payload.

## Triage guidance

The verifier removes most wrong findings before you see them, but it
reads the same code you do and can be wrong in both directions. Verify
every surviving claim against the code before acting: don't dismiss a
real bug because it reads pedantic, and don't "fix" working code
because the finding sounds confident.

- **A finding can be right about the defect and wrong about the
  remedy.** Verify the remedy too. On #902 a finding correctly called a
  schema assertion too weak, then prescribed asserting on the enum's
  `enum` array — but `schemars` renders a documented fieldless enum as
  `oneOf[].const`, so the literal fix would have asserted against a key
  that does not exist. Dumping the actual artefact first cost one
  command.
- **A finding about a test is proved by the test failing.** Revert or
  break the behaviour the test guards and confirm the test fails before
  calling it fixed — that check has both proved remedies and proved
  declines. Fixing a test double can vacuum the tests built on it, so
  mutation-test after correcting one.
- **Sweep the class, not the site.** A stale reference or a missed
  registration site usually has siblings the round did not reach; on
  #1326 sweeping each finding's class turned up eleven more.
- **Decline with evidence** — a code pointer, a doc link, a command and
  its output — in the thread reply or the outcome comment.
- **A vague finding is held to the bar of nameable.** Look for a
  concrete gap it could mean; fix it if you find one, decline on the
  record if you cannot.

### What the record shows

The lenses and their order come from classifying all 1492 Copilot review
threads across PRs #142–#808 (230 PRs that drew comments). Share of a
category's comments that led to a real improvement:

| Category                         |   n | useful | harmful |
| -------------------------------- | --: | -----: | ------: |
| Races, locking, task lifetime    |  51 |    86% |      0% |
| Security                         |  57 |    83% |      2% |
| Validation / missing mirror site | 149 |    80% |      2% |
| Logic bugs                       | 213 |    78% |      9% |
| Error handling                   | 133 |    72% |      3% |
| Test quality                     | 161 |    58% |      6% |
| Doc / comment drift              | 562 |     9% |      3% |
| Style nits                       |  75 |     3% |      3% |
| "This won't compile"             |  32 |     0% |     84% |

The first five rows are where review caught defects nothing else
could; they map to the concurrency, safety, correctness and
silent-failures lenses. The test-quality row undersells its lens: on
the PRs where findings hidden in Copilot's review bodies were counted
(#902, #923, #1246, #1326), the sharpest catches were repeatedly tests
that could pass vacuously. Doc drift was 38 % of all volume and the
least productive, which is why the docs lens holds every comment to
*would following this text cause a wrong action?* Compile claims are
excluded outright. Shell and infra state machines also outperformed the
table — #857 drew five genuine bugs in new bash (crash/restart windows,
a failure mode aliased to "stopped") — which is why the concurrency and
correctness lenses read scripts as well as Rust.

## Tuning

- A lens is its agent file in `.claude/agents/`. Keep each one to its
  class: a lens that reports outside it duplicates another lens and
  spends a verifier on the duplicate.
- Adding a lens means an agent file, an entry in the workflow's
  `LENSES` table with its path trigger, and a row in the table above.
- When a lens keeps producing findings the verifier refutes, tighten
  the lens; when a class of real defect reaches merge unreported, that
  is a missing lens or a missing line in one.

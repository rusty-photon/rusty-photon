# Skill: Code Review

## When to Read This

- Reviewing a pull request — while babysitting it
  ([babysitting-prs.md](babysitting-prs.md)) or on its own
- Triaging review findings, the review's or a human reviewer's
- Changing how this repo reviews pull requests

## Running a review

Reviews use Claude Code's built-in `/code-review` command, run from a
checkout of the PR's head commit — the reviewer reads the working tree
for the context around the diff:

```text
/code-review high <pr> --comment
```

An agent runs the same thing with the Skill tool: `skill: "code-review"`,
`args: "high <pr> --comment"`. `--comment` posts each finding as an
inline comment on the PR, under the account `gh` is logged in as. A
finding it cannot anchor to a line of the diff is printed instead; it
goes in the round comment (babysitting-prs.md, step 2 of the loop).

Claude Code runs the review in a separate context, so it carries none of
the calling session's reasoning about the change. That independence from
the author is most of what a review is worth.

**Level: `high`**, chosen on 2026-10-10 as the starting point; revisit
it once a few PRs have been through. The level trades cost for recall.
With Opus 5.5 on Claude Code 2.1.296:

| Level            | What runs                                                                   | Reports |
| ---------------- | --------------------------------------------------------------------------- | ------: |
| `low`            | one pass over the diff hunks, test files skipped, no verification          |    ≤ 4 |
| `medium`, `high` | one agent works through eight review angles and dedups; no verification    |   ≤ 10 |
| `xhigh`          | one agent works through ten angles, then a gap sweep; no verification      |   ≤ 15 |
| `max`            | ten independent finder agents, a verifier per candidate, then a gap sweep   |   ≤ 15 |

What runs depends on the model and the Claude Code version, so check
before relying on this table: the review's prompt opens with a one-line
summary of its recipe, visible in the review agent's transcript. The
`high` review of #1459 on 2026-10-10 opened with
`high effort → 8 inline angles → dedup (no verify) → ≤10 findings`; it
took about three minutes and 94k tokens. `--max-findings <n>|all`
raises the cap. `/code-review ultra`, a cloud
review, can only be launched by a person. If real defects keep reaching
merge past `high`, move the default up and record why here.

Without Claude Code, any careful reviewer works: give them the section
below, and triage what they find the same way.

## What to look for here

AGENTS.md rule 15 carries the short form of this section to every
reviewer, `/code-review` included, because CLAUDE.md is the one file
Claude Code loads into every review.

**Anchor every finding to what the PR is for.** A finding must bear on
whether the PR achieves its purpose, or breaks something on the way; it
is not a licence to audit the surrounding system or to ask for work the
PR defers. **Review a plan as a plan** (`docs/plans/`): contradictions,
false claims about the code, a step that cannot work — not lock
ordering, timeouts or exact APIs that prose is not meant to carry.

The defect classes review has actually caught in this repo (§What the
record shows), most productive first:

- **Hardware actuation on connect** — project tenet 3 (AGENTS.md rule
  13): no path from startup, connect/reconnect, config apply or a
  supervisory transition may move, home, park, toggle power or change a
  setpoint.
- **Races and lifetimes** — guards held across `.await`, detached tasks,
  error paths that skip rollback, missing timeouts; in shell and infra
  state machines too (crash and restart windows).
- **Security** — secrets, injection, escaping, unvalidated paths,
  internal addresses in a public repo.
- **The other site that needs the same change** — a config key, device,
  variant or route added in one place but not its mirrors (schema, docs,
  UI, BDD, packaging).
- **Silent wrongness** — values dropped, coerced or defaulted instead of
  failing; units and casts; swallowed errors and unjustified fallbacks.
- **Tests that cannot fail** — assertions pre-satisfied by setup,
  degenerate fixtures, scenarios no step exercises.
- **CI and packaging** — swallowed failures, publish ordering, wiring
  that misses a platform or a workflow.
- **Docs** — only text that would make a reader take a wrong action, and
  design docs a behaviour change left stale (AGENTS.md rule 2).

Not findings:

- predicted build, lint or format failures — CI runs Bazel, both clippy
  passes and `cargo fmt`, and such claims were 84 % harmful here;
- style, naming and phrasing;
- claims about external behaviour (udev, systemd, GitHub Actions, shell,
  a crate's semantics) that don't quote the manual or the source;
- a sleep, retry or readiness loop to make code tolerate a race — this
  repo rejects those as masking the defect; report the race;
- defects in code the PR neither changed nor depends on — those are
  pre-existing: an issue, not a finding against this PR.

## Triage

`high` has no separate verification step, so expect some findings to be
wrong. Verify every claim against the code before acting, in both
directions: don't dismiss a real bug because it reads pedantic, and don't
"fix" working code because the finding sounds confident.

- **A finding can be right about the defect and wrong about the
  remedy.** On #902 a finding correctly called a schema assertion too
  weak, then prescribed asserting on the enum's `enum` array — but
  `schemars` renders a documented fieldless enum as `oneOf[].const`.
  Dumping the actual artefact first cost one command.
- **A finding about a test is proved by the test failing.** Break the
  behaviour the test guards and confirm it fails before calling it
  fixed. Fixing a test double can vacuum the tests built on it, so
  mutation-test after correcting one.
- **Sweep the class, not the site.** A stale reference or a missed
  registration site usually has siblings; on #1326 sweeping each
  finding's class turned up eleven more.
- **Decline with evidence** — a code pointer, a doc link, a command and
  its output — in the thread reply.
- **A vague finding is held to the bar of nameable.** Look for a
  concrete gap it could mean; fix it if you find one, decline on the
  record if you cannot.
- **A finding declined before can come back:** each review sees the PR
  as it stands, not earlier rounds. Reply with a link to the earlier
  decline instead of arguing it again.

## What the record shows

From classifying all 1492 Copilot review threads across PRs #142–#808
(230 PRs that drew comments) — the share of each category's comments
that led to a real improvement:

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

The test-quality row undersells itself: on the PRs where findings hidden
in Copilot's review bodies were counted (#902, #923, #1246, #1326), the
sharpest catches were repeatedly tests that could pass vacuously. Doc
drift was 38 % of all volume and the least productive. Shell and infra
state machines outperformed the table — #857 drew five genuine bugs in
new bash.

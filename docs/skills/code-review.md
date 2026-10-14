# Skill: Code Review

## When to Read This

- Reviewing a pull request — while babysitting it
  ([babysitting-prs.md](babysitting-prs.md)) or on its own
- Rating and acting on review findings, the review's or a human reviewer's
- Changing how this repo reviews pull requests

## Running a review

Reviews use Claude Code's built-in `/code-review` command, run from a
checkout of the PR's head commit — the reviewer reads the working tree
for the context around the diff:

```text
/code-review high <pr> --max-findings all
```

An agent runs the same thing with the Skill tool: `skill: "code-review"`,
`args: "high <pr> --max-findings all"`.

- No `--comment`. The findings come back to the session that ran the
  review, which has them rated (§Rating a finding) and records every one,
  with its rating and outcome, in a single PR comment per review
  (babysitting-prs.md, step 8 of the loop). Posting each finding as an
  inline thread cost a reply apiece and spread one round's outcome over
  dozens of threads — about 40 per PR across the first 22 PRs.
- `--max-findings all` keeps every finding. Without it the review stops
  at its level's cap (10 at `high`), or at whatever cap someone last
  typed, which the skill remembers. What matters is decided by the
  rating, not by a cap.

Claude Code runs the review in a separate context, so it carries none of
the calling session's conversation about the change. That independence
from the author is most of what a review is worth. It does load
CLAUDE.md and, on a machine with Claude Code auto-memory, the user's
memory index, which can hold notes about the PR under review.

**Level: `high`**, chosen on 2026-10-10 as the starting point and kept
after the first 22 PRs (§What the record shows). The level trades cost
for recall. With Opus 5.5 on Claude Code 2.1.296:

| Level            | What runs                                                                   | Default cap |
| ---------------- | --------------------------------------------------------------------------- | ----------: |
| `low`            | one pass over the diff hunks, test files skipped, no verification          |          4 |
| `medium`, `high` | one agent works through eight review angles and dedups; no verification    |         10 |
| `xhigh`          | one agent works through ten angles, then a gap sweep; no verification      |         15 |
| `max`            | ten independent finder agents, a verifier per candidate, then a gap sweep   |         15 |

What runs depends on the model and the Claude Code version, so check
before relying on this table: the review's prompt opens with a one-line
summary of its recipe, visible in the review agent's transcript. The
first `high` review of #1459 on 2026-10-10, run before this repo added
`--max-findings all`, opened with
`high effort → 8 inline angles → dedup (no verify) → ≤10 findings`; it
took about three minutes and 94k tokens. With `--max-findings all` the
line ends in `all findings` instead. Claude can't launch
`/code-review ultra`, the cloud review; a person types it. If real
defects keep reaching merge past `high`, move the default up and record
why here.

The first 22 PRs showed a relevance problem, not a recall one: the
review was rarely wrong, but two thirds of what it raised had no
realistic production scenario. A higher level does not address that —
`max`'s extra step checks whether a finding is true — so realism is
applied after the review, by §Rating a finding.

Without Claude Code, any careful reviewer works: give them the section
below, and rate and act on what they find the same way.

## What to look for here

AGENTS.md rule 15 carries the short form of this section to every
reviewer, `/code-review` included, because CLAUDE.md is the one file
Claude Code loads into every review. A review gets the CLAUDE.md its
session loaded when it started, so a change to the rules reaches reviews
from the next session on.

**Anchor every finding to what the PR is for.** A finding must bear on
whether the PR achieves its purpose, or breaks something on the way; it
is not a licence to audit the surrounding system or to ask for work the
PR defers. **Review a plan as a plan** (`docs/plans/`): contradictions,
false claims about the code, a step that cannot work — not lock
ordering, timeouts or exact APIs that prose is not meant to carry.
**One finding per defect:** raise it on the clearest or authoritative
instance and name the other locations in it. **Converge:** text or code
added to answer an earlier finding is not new risk; raise a finding on
it only if it is wrong, not because it could say more.

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

## Rating a finding

The review reports what it can see, and nearly all of it is true — 2 %
of the findings in §What the record shows were wrong. What it does not
do is weigh them. So every finding is rated for realism and severity
before anyone acts on it, and the rating, not whether the finding is
true, decides what happens to it. A true finding that no realistic
sequence of events triggers is declined.

### What production means here

rusty-photon runs on a Raspberry Pi 5 at the telescope and on a Windows
NUC at a remote observatory: one operator, a private LAN, unattended
night sessions, and the tenets in
[workspace.md](../workspace.md#project-tenets). It is pre-1.0, with no
outside API consumers to protect.

- **Service code** — production is a night session on that rig.
- **Tests, BDD and CI** — production is CI on Linux, macOS and Windows.
  A real problem is a test that flakes, hangs, or cannot catch the
  regression it guards; CI that goes green while broken; a broken
  release or packaging build.
- **Docs, plans, validation records and skills** — a real problem is
  text that would make a reader take a wrong action, build the wrong
  thing, or believe a false claim about hardware evidence. Wording,
  completeness and "could say more" are not.

### Realism

- **likely** — happens in normal operation of the rig or in normal CI
  runs, or with ordinary operator actions: a restart, a config change, a
  cancelled sequence, a slow Windows runner. For a plan: the design's
  normal flow hits it.
- **plausible** — needs an uncommon but real event: a cable pull, a
  device fault or firmware quirk, a crash or power cut at a particular
  moment, an unusual but supported config, a loaded host.
- **remote** — needs several unlikely things at once, an exact timing
  boundary, adversarial input on the trusted LAN, or a config or
  platform nobody runs.
- **none** — no production scenario at all: reuse, simplification,
  efficiency, duplication, log levels, style, rule-citing ("rule 7 asks
  for `unwrap`"), and doc or test polish that changes no reader's action
  and no test's ability to fail.
- **wrong** — the claim is false: the code, a command or a record
  refutes it.

### Severity, if it happens

- **high** — wrong or lost science data, hardware actuation or damage,
  a night session wedged or lost, a security exposure, CI green while
  broken.
- **medium** — an operation fails with a clear error, a flaky or vacuous
  test, a doc that costs a reader real time or a wrong build step.
- **low** — cosmetic, log noise, a small inefficiency, a confusing
  message.

### Verdict

| Rating                                          | Verdict      | What happens to it                                                                                                                      |
| ----------------------------------------------- | ------------ | --------------------------------------------------------------------------------------------------------------------------------------- |
| likely or plausible, and medium or high         | **fix**      | fixed this round                                                                                                                        |
| likely or plausible and low, or remote and high | **optional** | fixed only if the change is small and stays in code the PR already touches; otherwise declined, in the round comment for the owner to see |
| anything else                                   | **decline**  | declined, with the rating's reason as the record                                                                                        |

A finding that repeats one an earlier round declined is declined with a
link to that round, unless the code it points at has changed since.
Fallout from an earlier round's fix is rated on its own merits, like any
other finding.

Anchors, all from the record:

| Finding                                                                                                                                                           | Rating                  |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------- |
| The SDK names the camera `SVBONY SV605CC` but the normaliser keys `SV605CC`, so a listed SV605CC is always served as a placeholder (#1466)                        | likely · high · fix     |
| After skipping a same-slot write, `set_filter` trusts `Position`, so a wheel still homing after 12 V returns reads as arrived: a frame through the wrong filter (#1472) | plausible · high · fix  |
| Both cancel tests cancel before the call starts, so deleting the cancel arm still passes (#1472)                                                                  | likely · medium · fix   |
| A plan's rule to dispatch workflows on the branch, applied to `nightly-packages.yml` without `dry_run`, publishes an unmerged branch as the public nightly (#1506) | likely · high · fix     |
| Bazel's action environment leaves `/usr/sbin` off `PATH`, so the new macOS test fails on every run (#1500)                                                        | likely · medium · fix   |
| READMEs and changelogs hard-code the MSRV, which goes stale on the next bump (#1477)                                                                              | likely · low · optional |
| A 600 s capture admitted at the very end of a guide-training lease holds the mount gate past the stated bound; real guide captures take seconds (#1509)           | remote · high · optional |
| HFR is computed twice per star: 0.2–0.5 ms against 1.4 s of detection (#1473, raised eight times)                                                                 | none · low · decline    |
| `spawn_service` makes two blocking-pool round trips where one would do (#1510)                                                                                    | none · low · decline    |
| A startup log line uses `info!` where rule 9 asks for `debug!` (#1507)                                                                                            | none · low · decline    |
| After two failed lease renewals, the third lands exactly on the TTL boundary and races the expiry (#1509)                                                         | remote · medium · decline |
| A `noserial-<port>` key containing dots breaks ui-htmx's error mapping — it matches errors by exact path (#1501)                                                  | wrong · low · decline   |

### The rating step

The session that wrote the PR does not rate its findings. On the record
it agreed with the review too readily: it fixed 421 of the 612
decline-rated findings that have a recorded outcome, and those fixes
were where much of the next round's fallout came from. After each
review, one fresh subagent rates every finding — in Claude Code, the
Agent tool with `general-purpose`, never a `fork`, which would inherit
the author's context. It reads the head from the checkout, so change
nothing there while it runs. Its prompt:

```text
Rate the findings of a code review of rusty-photon PR #<n>, head <sha>.
You did not write this PR, and you rate; you do not fix. Read
docs/skills/code-review.md §Rating a finding first and apply it exactly.
Judge each finding's scenario — what triggers it on the rig or in CI, and
what someone would see — not its tone, and not how easy it would be to
fix. Read the PR's earlier round comments (gh pr view <n> --comments) so
that a repeat of an earlier decline is rated as one. The checkout is at
the head; read the code only where a rating hinges on a fact the finding
does not settle. Change nothing. Answer one line per finding:
<k>. <realism> · <severity> · <verdict> — <trigger and consequence, at most 30 words>

Findings:
1. <path>:<line> — <the finding's full text>
2. …
```

The babysitter acts on the verdicts, and overrules one only with
evidence, saying so in the round comment. The same evidence counts in
both directions: the code, a command and its output, or a record. A
**fix** whose claim that evidence refutes is declined; a **decline**
whose trigger it shows to be realistic — the path runs on every
reconnect, a reproduction, a log, hardware behaviour on record — is
fixed. That a finding sounds right, or that its fix looks cheap, is
evidence of neither.

A human reviewer's comment is not rated: it gets an answer on its
merits, in its own thread.

## Acting on a finding

`high` has no separate verification step, and the rating judges a
finding's scenario rather than re-reading the code. So check every
finding against the code before acting on it, in both directions — a
fix-rated claim before fixing it, a decline-rated one for the trigger
its rating ruled out: don't dismiss a real bug because it reads
pedantic, and don't "fix" working code because the finding sounds
confident.

- **A finding can be right about the defect and wrong about the
  remedy.** On #902 a finding correctly called a schema assertion too
  weak, then prescribed asserting on the enum's `enum` array — but
  `schemars` renders a documented fieldless enum as `oneOf[].const`.
  Dumping the actual artefact first cost one command.
- **A claim that code won't compile, lint or format is settled by the
  gate, not by argument** — run it. 27 of the 32 such claims in the
  Copilot record were factually wrong.
- **A finding about a test is proved by the test failing.** Break the
  behaviour the test guards and confirm it fails before calling it
  fixed. Fixing a test double can vacuum the tests built on it, so
  mutation-test after correcting one.
- **Sweep the class, not the site.** A stale reference or a missed
  registration site usually has siblings; on #1326 sweeping each
  finding's class turned up eleven more.
- **Record a decline's evidence** — a code pointer, a doc link, a
  command and its output — in the round comment, when it goes beyond the
  rating's reason.
- **A vague finding is held to the bar of nameable.** It is rated on the
  concrete gap it could mean, and declined if no such gap can be named.
- **A finding declined before can come back:** each review sees the PR
  as it stands, not earlier rounds. The rating step reads the earlier
  round comments and rates it as a repeat; its outcome links the earlier
  round instead of arguing it again.

## What the record shows

### `/code-review high`, 2026-10-10 to 10-14

Every finding `/code-review high` raised on the 22 PRs it reviewed in
its first four days — 993, on #1459 to #1510 — was rated afterwards
against §Rating a finding, one rater per batch of PRs. A blind second
rating of 35 of them agreed with the first on 29, with the
disagreements running both ways.

| Realism   |   n | Share |
| --------- | --: | ----: |
| likely    |  78 |   8 % |
| plausible | 215 |  22 % |
| remote    | 388 |  39 % |
| none      | 294 |  30 % |
| wrong     |  18 |   2 % |

By verdict that is 134 fix (13.5 %), 181 optional (18 %) and 678
decline (68 %). Sixteen findings were realistic and high severity, and
those are where review pays for itself: a camera that could never be
served (#1466), frames shot through the wrong filter (#1472), a park
slew outside any session (#1465), an unmerged branch published as the
public nightly (#1506).

- **The useful findings taper; the count does not.** Fix-rated findings
  by round ran 46, 33, 20, 17, 6 and 12 (rounds 1 to 6 and later), yet
  every round raised six to twelve findings, because noise filled the
  gap: the decline-rated share rose from 60 % in round 1 to 76 % in
  round 4. By round 4, 36 % of findings repeated an earlier round and
  59 % targeted code an earlier fix had added.
- **Fixing noise kept the rounds coming.** The authoring session fixed
  421 of the 612 decline-rated findings with a recorded outcome, and
  fixes breed findings: 30 of the 134 fix-rated findings, and 5 of the
  16 realistic high-severity ones, were in code an earlier round's fix
  had added. On #1472 the review proposed skipping a same-slot write to
  save 100 ms; it was applied, and the next round found that it let a
  homing wheel read as arrived.
- **Plans and docs repay review best.** The fix-rated share was 23 % on
  plans and 18 % on docs, against 14 % on tests, 11 % on code and 9 % on
  CI. Three PRs — #1470, #1476 and #1478, 89 findings over 14 rounds —
  drew no fix-rated finding at all.
- Under these verdicts, 15 of the 21 PRs that ran four or more reviews
  would have reached a clean review within the four-review budget,
  against one (#1478) under the earlier "fix anything legitimate".

### Copilot, PRs #142–#808

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

(Performance and uncategorized comments, 59 threads, are omitted.)

The test-quality row undersells itself: on the PRs where findings hidden
in Copilot's review bodies were counted (#902, #923, #1246, #1326), the
sharpest catches were repeatedly tests that could pass vacuously. Doc
drift was 38 % of all volume and the least productive. Shell and infra
state machines outperformed the table — #857 drew five genuine bugs in
new bash.

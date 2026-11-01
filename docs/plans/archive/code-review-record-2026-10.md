# The code-review record, October 2026

A record: two analyses of what review found on this repository, moved
whole from `docs/skills/code-review.md` on 2026-11-01. The skill's rules
rest on their numbers; nothing here is updated, and a later analysis is
a new record.

## `/code-review high`, 2026-10-10 to 10-14

Every finding `/code-review high` raised on the 22 PRs it reviewed in
its first four days — 993, on #1459 to #1510 — was rated afterwards
against code-review.md §Rating a finding, one rater per batch of PRs. A blind second
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
served (#1466), frames shot through the wrong filter (#1472), an
unmerged branch published as the public nightly (#1506).

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

## Copilot, PRs #142–#808

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

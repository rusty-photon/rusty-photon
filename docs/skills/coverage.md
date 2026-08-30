# Skill: Checking code coverage

Coverage gates every PR. [`bazel-coverage.yml`](../../.github/workflows/bazel-coverage.yml)
runs `bazel coverage`, splits the combined report per package for the
`bazel-coverage-lcov` artifact, uploads the combined report — minus the
coverage-exempt files — to **Coveralls** (the sole coverage vendor), and posts
the `uncovered-diff-lines` check on the PR. The moving parts:

| Check | What it asserts | State |
|---|---|---|
| `bazel coverage` | the instrumented suite passed and a report was published | **required** on `main` |
| Coveralls status | repo-wide coverage did not drop past the threshold set in the Coveralls repo settings | settings-side; not yet posting — see below |
| `uncovered-diff-lines` | nothing — informational: counts + inline annotations for the diff's uncovered lines | posts on every PR |

`bazel coverage` is required on `main` (the `main_protection` ruleset,
alongside `stable / fmt`, `stable / clippy` and `bazel /
{ubuntu,windows}-latest`). The standing rule when any coverage signal goes red
is **write the test**.

## The gating policy

The intended policy is a ratchet: **repo-wide coverage only goes up**, unless
the maintainer consciously approves a drop. The enforcement point is the
Coveralls repo settings (they act on the upload this workflow sends):

- **USE STATUS UPDATES** must be on for Coveralls to post commit
  statuses/checks at all. As of 2026-08-30 it posts none — comments only —
  so flipping this is the first step.
- **COVERAGE DECREASE THRESHOLD FOR FAILURE** is the ratchet. Prefer a small
  non-zero value (0.05–0.1) over 0: run-to-run instrumentation jitter is
  real, and a 0 threshold reddens on noise.
- **COVERAGE THRESHOLD FOR FAILURE** is an absolute floor, secondary to the
  ratchet.

A required check that never reports stays pending forever and blocks
**every** PR, so the Coveralls status joins the ruleset only after being
observed posting on several PRs (the same observe-first discipline every
vendor check here has gone through). Confirm with both API shapes, because a
vendor may post either commit statuses or check runs:

```bash
gh api 'repos/{owner}/{repo}/commits/<sha>/status' --jq '.statuses[] | "\(.context) \(.state)"'
gh api 'repos/{owner}/{repo}/commits/<sha>/check-runs' \
  --jq '.check_runs[] | "\(.app.slug) \(.status) \(.conclusion // "-") \(.name)"'
```

Approving a drop, once the status is required, is an admin bypass of that
check — visible in the PR timeline — made after reading what §0/§1 say is
uncovered.

## What is (and is not) in the numbers

`bazel coverage` is the **sole** coverage source: there is no Cargo coverage
job, and the nightly Cargo safety net (`test.yml`) deliberately collects none.

The only files outside the published numbers are test directories, mock
binaries, test-helper binaries, and examples. That policy lives in one place —
`IGNORED_RE` in [`filter_lcov.py`](../../tools/coverage/filter_lcov.py) —
which strips those records from the report uploaded to Coveralls; the diff
scoring inherits the policy by consuming the filtered report. Adding
production code to that pattern to make a number go green is not an
available fix; **write the test**.
A `#[cfg(test)] mod tests` block is already excluded by the `coverage(off)`
attribute the nightly toolchain honours (see `.bazelrc` `--config=coverage`),
so nothing you write in one distorts the result. The CI artifact (§1) keeps
the *unfiltered* report, so the exempt files remain inspectable.

## History: the Codecov era (through 2026-08-30)

Codecov was the vendor before Coveralls, and three of its lessons are kept so
nobody re-derives them:

- Its required `codecov/patch` check enforced diff-coverage ≥ the base's
  project coverage (`target: auto`) — the same ratchet intent the Coveralls
  decrease threshold now carries. **When the Coveralls migration merged, the
  `codecov/patch` entry had to leave the `main_protection` ruleset**, or every
  PR would block on a check that never reports again.
- `codecov/project` stopped posting when the repo moved to the
  `rusty-photon` org (2026-08-02): the free tier does not include Project
  Coverage. It was never a configuration problem.
- The 21 per-service flag badges spent 08-22→08-30 all rendering the
  identical repo-wide number: the codecov CLI ran without `--disable-search`,
  so the `combined-coverage.info` file created for Coveralls was auto-added
  to **every** flag upload. Per-service badges were removed with the
  migration (Coveralls has no per-flag badges); per-file detail lives in the
  Coveralls UI instead.

Pre-transfer history (through commit `bf00fe3c`) is frozen in the
`ivonnyssen/rusty-photon` Codecov project; 2026-08-02→2026-08-21 has no
coverage data anywhere. Nothing should link either Codecov project any more.

## Which route answers which question

| Question | Route |
|---|---|
| *What does this PR leave uncovered?* — while reviewing it | the `uncovered-diff-lines` check annotations, inline in the PR's Files changed tab (§0) |
| *Which exact lines did CI find uncovered?* — any branch, scripted | the CI artifact (§1) |
| *What is the number, per file / branch / build?* | the Coveralls API and UI (§2) |
| *Am I about to push a regression?* | reproduce locally (§3) |

## 0. In-diff annotations — uncovered lines where review happens

On every PR, the coverage job posts a **non-gating** check run named
`uncovered-diff-lines` on the PR head: the title carries the diff's coverage
percentage and finding counts, and one annotation per run of uncovered added
lines renders inline in the **Files changed** tab, plus a file-level
annotation (at the file's first added line) for any changed first-party `.rs`
file with no coverage record at all. The scoring is
[diff-cover](https://github.com/Bachmann1234/diff_cover)'s (pip-pinned in the
workflow): it intersects the filtered report with the PR's merge-base diff
fetched from the GitHub API (`--diff-file`, so no git history is needed on
the shallow CI checkout), and `post_check_annotations.py` converts its JSON
findings into Checks API payloads — so the check agrees with the published
Coveralls number by construction, and no vendor is involved. The
no-coverage-record finding is the one thing diff-cover cannot report (it
silently skips such files); the poster recovers it by re-checking the diff's
changed files against the report's `SF:` paths.

Semantics worth knowing:

- **It never gates.** Conclusion is `success` when every added line is
  covered (or the diff has no coverable lines) and `neutral` when findings
  exist. Promoting it to a required check would be a branch-protection
  decision, made the usual observe-first way.
- **It only exists where the coverage run succeeded** — the posting step runs
  after the split, which a failed `bazel coverage` skips, matching the
  don't-publish-failed-runs rule above.
- **Fork PRs don't get it**: their `GITHUB_TOKEN` is read-only, the API
  refuses the check-run write, and the step degrades to a run warning — as
  does every other failure in the step, because visibility must never redden
  the required `bazel coverage` check.
- **Annotations are capped at 200** (50 per API request, appended in
  chunks). Past the cap the check's summary counts what was cut; the full
  list is always in the artifact.
- **Rendering has a platform ceiling.** GitHub gives third parties only
  these annotation boxes — no one can tint lines inside the Files changed
  tab. Whole-file line-level rendering lives in the Coveralls UI (§2).

The pipeline is runnable locally: `pip install diff-cover`, produce the JSON
report (`diff-cover <report> --diff-file <diff> --format json:dc.json
--total-percent-float`), and `post_check_annotations.py dc.json --diff <diff>
--lcov <report> --dry-run …` prints the API requests it would make (actually
posting needs an Actions token — only GitHub Apps can create check runs, so
a user PAT cannot).

## 1. The CI artifact — line-level truth

`bazel-coverage.yml` uploads `coverage-lcov/*.info` as the artifact
**`bazel-coverage-lcov`**: one `lcov-<pkg>.info` per workspace package, named
for the directory basename under `crates/` or `services/`. This is the same
data Coveralls ingests (before the exempt-file filter), so it is the
authority when a published number and your intuition disagree.

```bash
run=$(gh run list --workflow=bazel-coverage.yml --branch <branch> \
        --limit 1 --json databaseId --jq '.[0].databaseId')
gh run download "$run" -n bazel-coverage-lcov -D /tmp/cov
```

`gh run download` must run from inside the repository, but `-D` may point
anywhere. Artifacts expire after 90 days, so this route covers recent runs only;
past that, re-run the workflow or fall back to §2.

Then intersect the report with the diff — the question worth asking, because
whole-file percentages say nothing about whether *your* lines are tested.
The scorer is [diff-cover](https://github.com/Bachmann1234/diff_cover)
(`pip install diff-cover`); filter the artifact first so the exempt files
stay out, exactly as CI does:

```bash
python3 tools/coverage/filter_lcov.py /tmp/cov/*.info --output /tmp/combined.info
diff-cover /tmp/combined.info --compare-branch origin/main --show-uncovered
```

It prints per-file diff coverage with the missing line numbers and the
diff-wide percentage; `--fail-under 100` makes it exit non-zero when anything
is uncovered, and `--diff-file <file>` scores a diff you already have instead
of running git. One finding it cannot report — a changed first-party `.rs`
file with **no** coverage record at all (nothing instrumented it, a stronger
finding than a few missed lines) — is recovered by
`post_check_annotations.py`, which is what CI posts.

For an unfiltered view of a single file, read the `DA:<line>,<hits>` records
directly — a `0` hit count is a missed line:

```bash
awk -F'[:,]' '/^SF:/{f=$2} /^DA:/ && $3==0 {print f, $2}' /tmp/cov/lcov-doctor.info
```

## 2. The Coveralls API and UI — totals, no token

The repo is public, so the Coveralls API answers unauthenticated. Verified
endpoints:

| Endpoint | Returns |
|---|---|
| `https://coveralls.io/github/rusty-photon/rusty-photon.json` | repo summary: latest build, `covered_percent`, badge URL |
| `…/rusty-photon.json?branch=main` | the same, scoped to a branch |
| `https://coveralls.io/builds/<id>.json` | one build: `covered_percent`, `coverage_change`, commit metadata |

`/builds.json` (a builds listing) is **not** available (404). Build ids come
from the repo summary, from the PR comment ("Coverage Report for CI Build
…" links `coveralls.io/builds/<id>`), or from the Coveralls UI.

The UI is the line-level view: a build's page renders **whole files** with
per-line hit counts — not just the diff — which is what the in-repo tooling
cannot draw inside GitHub (§0's ceiling). Coveralls also comments on every PR
with the patch coverage, the change vs. the base build, and an "Uncovered
Changes" list, so `gh pr view <n> --comments` is often the fastest look of
all.

The badge is
`https://coveralls.io/repos/github/rusty-photon/rusty-photon/badge.svg?branch=main`.
Check a badge by its rendered text, not its HTTP status — a badge with no
data still returns 200 with "unknown" painted in it:

```bash
curl -sL <badge-url> | grep -oE '>[^<]*</text>'
```

## 3. Reproducing coverage locally

Before pushing, or when the artifact is gone:

```bash
bazel coverage --config=coverage //...                       # needs OmniSim (OMNISIM_PATH/OMNISIM_DIR)
python3 tools/coverage/filter_lcov.py \
  "$(bazel info output_path)/_coverage/_coverage_report.dat" --output /tmp/combined.info
diff-cover /tmp/combined.info --compare-branch origin/main --show-uncovered
```

`--config=coverage` selects the nightly channel and the instrumentation filter
that reproduce CI's contract; a plain `bazel coverage` does not. The filter
step reproduces the number Coveralls publishes. For the same per-package
files CI uploads as the artifact:

```bash
python3 tools/coverage/split_lcov.py \
  "$(bazel info output_path)/_coverage/_coverage_report.dat" --output-dir /tmp/cov
```

This is the heaviest item in the pre-push set. See [pre-push.md](pre-push.md).

## Gotchas

- **The Coveralls upload never fails the job** (`fail-on-error: false`): a
  vendor hiccup must not redden `bazel coverage`, which is a required check.
  The cost is that an upload failure leaves the badge and the PR comment
  stale until the next green run — read the run's log/annotations before
  concluding a number moved.
- **Coveralls compares against the PR's base build.** A base commit with no
  successful upload makes the "coverage changed by X%" verdict meaningless
  rather than red — its comment names the base build it compared against;
  confirm that build exists before chasing a phantom drop.
- **The published number excludes the exempt files; the artifact does not.**
  A hand-computed percentage from the raw artifact will sit slightly off the
  Coveralls number — `filter_lcov.py` (§3) closes the gap.
- **Doctests are largely outside the gate.** rules_rust only runs the crates
  that declare a `rust_doc_test` target, so lines reached solely from a doc
  example are counted uncovered. Cover them with a real test.
- **LCOV records only instrumented lines.** An added line missing from the
  report is not code (blank, comment, brace) — diff-cover treats it as
  neither covered nor missed.

# Skill: Checking code coverage

Coverage gates every PR. [`bazel-coverage.yml`](../../.github/workflows/bazel-coverage.yml)
runs `bazel coverage`, splits the combined report per package, and uploads each
package under its own Codecov flag; Codecov then posts two checks:

| Check | What it asserts |
|---|---|
| `codecov/patch` | the lines **this PR adds or changes** are covered |
| `codecov/project` | repo-wide coverage did not drop more than `threshold: 1%` |

`codecov/patch` is **required** on `main` (the `main_protection` ruleset, app
id 254, alongside `stable / fmt`, `stable / clippy`,
`bazel / {ubuntu,windows}-latest` and `bazel coverage`). The standing rule when
it goes red is **write the test**.

`codecov/project` is **not** required, deliberately. It scores a PR against the
base commit's report on `main`, so it posts nothing at all when that base
predates the post-transfer project's first ingested `main` commit
(2026-08-21 — see the gap note below). A required check that never reports
stays pending forever and blocks every PR, so it stays advisory until it has
been observed posting on a PR whose base `main` commit Codecov holds. Confirm
with the check-runs API before promoting it — Codecov posts these as **check
runs** from the `codecov` app, not as commit statuses, so
`commits/<sha>/status` shows nothing:

```bash
gh api 'repos/{owner}/{repo}/commits/<sha>/check-runs' \
  --jq '.check_runs[] | select(.app.slug=="codecov") | "\(.conclusion) \(.name)"'
```

`bazel coverage` is the **sole** coverage source: there is no Cargo coverage
job, and the nightly Cargo safety net (`test.yml`) deliberately collects none.

The standing rule when a coverage check goes red is **write the test**. Adding
a path to `ignore:` in [`.github/codecov.yml`](../../.github/codecov.yml) to
make a number go green is not an available fix for shipping code; that list
covers test files, mock/test helper binaries, and examples, and it stays that
way. A `#[cfg(test)] mod tests` block is already excluded from the numbers by
the `coverage(off)` attribute the nightly toolchain honours (see `.bazelrc`
`--config=coverage`), so nothing you write in one distorts the result.

## Which route answers which question

Three routes exist. None needs an MCP server, a plugin, or a Codecov token —
the repo is public, so its Codecov API answers unauthenticated.

### The pre-transfer project is frozen — mind the gap

Codecov keys projects by owner slug, so the move to the `rusty-photon` org
([#842](https://github.com/rusty-photon/rusty-photon/pull/842)) split the history
in two:

| Project | Holds | State |
|---|---|---|
| `ivonnyssen/rusty-photon` | everything up to commit `bf00fe3c`, 2026-08-02 | frozen archive — never updates again |
| `rusty-photon/rusty-photon` | 2026-08-21 onward | live; every route below reads this one |

Between those dates the uploader kept reporting `status: queued` against the old
slug and nothing was ever processed, so **2026-08-02 → 2026-08-21 has no coverage
data in either project** — no badge, no totals, no `compare/` entry. Do not read a
regression into a number that straddles that window; there is no base to compare
against. The gap closed when the Codecov GitHub App was installed on the org,
which is what re-pointed the uploader at the new slug.

Two consequences worth knowing:

- The old project's badges still render, and still say `94%`. That is 2026-08-02's
  number, not today's. Nothing in this repo should link them.
- **Check a badge by its rendered text, not its HTTP status.** Both slugs return
  HTTP 200 for `graph/badge.svg`; a project with no data returns 200 with the word
  `unknown` painted in it. `curl -s <badge-url> | grep -oE '>[^<]*</text>'` is the
  test that actually distinguishes them.

| Question | Route |
|---|---|
| *Which exact lines did CI find uncovered?* | the CI artifact (§1) — the only route with line numbers |
| *Will `codecov/patch` pass?* | the artifact plus [`uncovered_in_diff.py`](../../tools/coverage/uncovered_in_diff.py) (§1) |
| *What is the number, per file / flag / PR?* | the Codecov API (§2) |
| *Am I about to push a regression?* | reproduce locally (§3) |

## 1. The CI artifact — line-level truth

`bazel-coverage.yml` uploads `coverage-lcov/*.info` as the artifact
**`bazel-coverage-lcov`**: one `lcov-<pkg>.info` per workspace package, named
for the directory basename under `crates/` or `services/` (that basename is
also the Codecov flag). This is the same data Codecov ingests, before Codecov
summarises it — so it is the authority when a check and your intuition disagree.

```bash
run=$(gh run list --workflow=bazel-coverage.yml --branch <branch> \
        --limit 1 --json databaseId --jq '.[0].databaseId')
gh run download "$run" -n bazel-coverage-lcov -D /tmp/cov
```

`gh run download` must run from inside the repository, but `-D` may point
anywhere. Artifacts expire after 90 days, so this route covers recent runs only;
past that, re-run the workflow or fall back to §2.

Then intersect the report with the diff — this is the local equivalent of
`codecov/patch`, and it is the question worth asking, because whole-file
percentages say nothing about whether *your* lines are tested:

```bash
python3 tools/coverage/uncovered_in_diff.py /tmp/cov --base origin/main
```

It prints the added lines LCOV instrumented and no test executed, flags a
changed first-party `.rs` file that has **no** coverage record at all (nothing
instrumented it — a stronger finding than a few missed lines), skips whatever
`.github/codecov.yml` ignores, and exits non-zero when anything is uncovered.
Pass `--diff <file>` (or `-` for stdin) to score a diff you already have rather
than one against the working tree.

For an unfiltered view of a single file, read the `DA:<line>,<hits>` records
directly — a `0` hit count is a missed line:

```bash
awk -F'[:,]' '/^SF:/{f=$2} /^DA:/ && $3==0 {print f, $2}' /tmp/cov/lcov-doctor.info
```

## 2. The Codecov API — totals, no token

Base URL: `https://api.codecov.io/api/v2/github/rusty-photon/repos/rusty-photon`.
Verified endpoints:

| Endpoint | Returns |
|---|---|
| `totals/?branch=main` | repo totals plus a per-file breakdown |
| `report/?path=<file>` | one file's `lines` / `hits` / `misses` / `coverage` |
| `compare/?pullid=<n>` | base-vs-head totals; entries with `has_diff: true` are the PR's files |
| `flags/` | the per-package flags |
| `commits/` | recent commits and their totals |

`report/tree/` is **not** available (404) — use `totals/` and read its `files`
array instead.

The changed-file summary for a PR, which is the most useful of these:

```bash
curl -s "https://api.codecov.io/api/v2/github/rusty-photon/repos/rusty-photon/compare/?pullid=<n>" \
  | python3 -c 'import json,sys
d = json.load(sys.stdin)
for f in d["files"]:
    if f.get("has_diff"):
        t = f["totals"]["head"]
        print(f"{f[\"name\"]:60} {t[\"coverage\"]:6}%  {t[\"misses\"]} missed")'
```

Codecov also comments on the PR (`comment.layout: files`), so
`gh pr view <n> --comments` is often the fastest look of all.

## 3. Reproducing coverage locally

Before pushing, or when the artifact is gone:

```bash
bazel coverage --config=coverage //...                       # needs OmniSim (OMNISIM_PATH/OMNISIM_DIR)
python3 tools/coverage/split_lcov.py \
  "$(bazel info output_path)/_coverage/_coverage_report.dat" --output-dir /tmp/cov
python3 tools/coverage/uncovered_in_diff.py /tmp/cov --base origin/main
```

`--config=coverage` selects the nightly channel and the instrumentation filter
that reproduce CI's contract; a plain `bazel coverage` does not. The split step
is optional — `uncovered_in_diff.py` accepts the combined `.dat` directly — but
it gives you the same per-package files CI uploads.

This is the heaviest item in the pre-push set. See [pre-push.md](pre-push.md).

## Gotchas

- **Flag carryforward is off** by design (the rationale is in
  `.github/codecov.yml`). A cancelled or failed coverage run therefore leaves
  per-service badges reading "unknown" until the next green run — that is not a
  coverage regression.
- **`round: down`, `precision: 1`.** A file at 94.98% reports 94.9%, so a check
  can sit a hair under a threshold you thought you cleared. (`range: 85..100`
  only colours the display; it gates nothing.)
- **The `project` check compares against the PR's base**, so a base commit with
  no successful coverage upload makes its verdict meaningless rather than red.
  Confirm the base has a report (`commits/`) before chasing a phantom drop.
- **Doctests are largely outside the gate.** rules_rust only runs the crates
  that declare a `rust_doc_test` target, so lines reached solely from a doc
  example are counted uncovered. Cover them with a real test.
- **LCOV records only instrumented lines.** An added line missing from the
  report is not code (blank, comment, brace) — `uncovered_in_diff.py` treats it
  as neither covered nor missed, matching Codecov.

# Bazel cutover — `main_protection` ruleset flip (manual step)

**Status: COMPLETE (archived 2026-06-25).** The flip was run after #399 merged
(2026-06-24): the `main_protection` ruleset (id `3342975`) now requires exactly the
seven Bazel-era contexts listed below, and the old Cargo contexts are removed.
Retained as the record of the exact command and the rollback recipe. Parent plan:
[bazel-migration.md](bazel-migration.md).

The Bazel-cutover PR (see [bazel-migration.md](bazel-migration.md), Phase 7)
changes the CI **workflows** so Bazel is the primary per-PR gate and the Cargo
build/test/coverage jobs run nightly. But which checks are *required* lives in the
`main_protection` repository **ruleset** (id `3342975`), not in any file in the
repo — so making the cutover real is a one-time ruleset edit that an admin runs
**out of band**.

> ⚠️ Until this flip is run, the old Cargo contexts (`ubuntu / stable`,
> `ubuntu / stable / features`, `coverage`) stay *required* but no longer report
> on PRs (their workflows moved to nightly), so PRs show those checks as
> "expected" and block. The admin can still merge via the ruleset bypass; running
> the flip removes the block for everyone.

## The change

| Context | Before | After | Source |
|---|---|---|---|
| `stable / fmt` | required | **required (kept)** | check.yml |
| `stable / clippy` | required | **required (kept)** | check.yml |
| `ubuntu / stable` | required | **removed** (→ nightly) | test.yml |
| `ubuntu / stable / features` | required | **removed** (→ nightly) | check.yml |
| `coverage` | required | **removed** (replaced by `bazel coverage`) | test.yml |
| `bazel / ubuntu-latest` | — | **added** | bazel.yml |
| `bazel / macos-latest` | — | **added** | bazel.yml |
| `bazel / windows-latest` | — | **added** | bazel.yml |
| `bazel coverage` | — | **added** | bazel-coverage.yml |
| `bazel/cargo target parity` | — | **added** | parity.yml |

All checks use `integration_id: 15368` (GitHub Actions). The context strings must
be entered **exactly** as above — they are the resolved job `name:` fields
(verified against live check-runs); GitHub does not prefix them with the workflow
name.

## Recommended sequence (no ungated window)

1. Open the cutover PR (done).
2. Wait for the PR's new required checks to go green:
   `bazel / ubuntu-latest`, `bazel / macos-latest`, `bazel / windows-latest`,
   `bazel coverage`, `bazel/cargo target parity`, `stable / fmt`, `stable / clippy`.
3. Run the flip command below. The PR is now gated by those (green) checks and
   merges normally; `main` is never left without a build/test gate.

The flip affects **all open PRs** immediately (Bazel has been running on every PR
as a shadow job, so they already have recent results to satisfy it).

## The command (admin, run once)

Reads the current ruleset, swaps only the required-status-checks list, and PUTs it
back (everything else — conditions, bypass actors, enforcement — is preserved):

```bash
gh api repos/rusty-photon/rusty-photon/rulesets/3342975 \
  | jq '{name, target, enforcement, conditions, bypass_actors, rules}
        | (.rules[] | select(.type=="required_status_checks").parameters.required_status_checks) |=
          [
            {context:"stable / fmt", integration_id:15368},
            {context:"stable / clippy", integration_id:15368},
            {context:"bazel / ubuntu-latest", integration_id:15368},
            {context:"bazel / macos-latest", integration_id:15368},
            {context:"bazel / windows-latest", integration_id:15368},
            {context:"bazel coverage", integration_id:15368},
            {context:"bazel/cargo target parity", integration_id:15368}
          ]' \
  | gh api -X PUT repos/rusty-photon/rusty-photon/rulesets/3342975 --input -
```

## Verify

```bash
gh api repos/rusty-photon/rusty-photon/rulesets/3342975 \
  --jq '.rules[] | select(.type=="required_status_checks")
        | .parameters.required_status_checks[].context'
```

Expect exactly the 7 "After = required" contexts above.

## Rollback

Re-run the command with the **original** five contexts (Cargo is unchanged and
still runs nightly, so reverting the ruleset fully restores the old gate):

```bash
gh api repos/rusty-photon/rusty-photon/rulesets/3342975 \
  | jq '{name, target, enforcement, conditions, bypass_actors, rules}
        | (.rules[] | select(.type=="required_status_checks").parameters.required_status_checks) |=
          [
            {context:"stable / fmt", integration_id:15368},
            {context:"stable / clippy", integration_id:15368},
            {context:"ubuntu / stable", integration_id:15368},
            {context:"ubuntu / stable / features", integration_id:15368},
            {context:"coverage", integration_id:15368}
          ]' \
  | gh api -X PUT repos/rusty-photon/rusty-photon/rulesets/3342975 --input -
```

(If rolling back the gate, also revert the workflow `on:` triggers so the Cargo
checks report on PRs again — e.g. `git revert` the cutover commit.)

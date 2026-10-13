---
allowed-tools:
  - Bash(act:*)
  - Bash(git branch:*)
  - Bash(basename:*)
  - Bash(cargo fmt:*)
  - Bash(cargo build:*)
  - Bash(cargo test:*)
  - Bash(cargo clippy:*)
  - Task
---

Run the full pre-push quality-gate suite by delegating to `act` (local GitHub
Actions runner). Use a **Task-based agent team** to run jobs in parallel without
overflowing context. Do NOT stop on the first failure — run all jobs and report
a summary at the end.

## Context

Current branch: `!git branch --show-current`
Worktree name: `!basename "$(git rev-parse --show-toplevel)"`
Arguments: $ARGUMENTS

## Architecture

Each `act` job runs inside its own **Task subagent** (subagent_type: `Bash`)
launched with **`run_in_background: true`**. Subagents are **read-only
investigators** — they run the job, diagnose any failure, and write a concise
synopsis to their output file. They NEVER edit or fix files.

Results stay in **output files**, not in the main context. The main agent (you)
coordinates the team, reads only the synopsis line from each output file to
build a summary table, and only reads full diagnosis details for jobs that
failed. This avoids context overflow and edit conflicts.

**Critical:** Always use `run_in_background: true` on every Task call. Never
use blocking Task calls — they inject the full subagent response into your
context and will cause context overflow with 10 parallel jobs.

**Container isolation:** Every `act` command MUST include
`--container-name-prefix=<worktree-name>-` (using the worktree name from the
Context section). This namespaces Docker containers per worktree so multiple
`/pre-push` runs on different worktrees of the same repo can run in parallel
without Docker container/volume conflicts.

## Steps

### 1. Verify branch

Confirm the current branch is NOT `main`. If it is, **stop immediately** and
tell the user to create a feature branch first (CLAUDE.md Rule 5).

### 2. Launch CI jobs via agent team

Spawn one Task subagent per job (or job chain). Launch all independent agents
simultaneously using the Task tool with `run_in_background: true`.

**Parallel group** — launch all of these simultaneously as separate Task agents.
Replace `{PREFIX}` with the worktree name from the Context section above:

| Agent label         | Command                                                                                  |
|---------------------|------------------------------------------------------------------------------------------|
| clippy (fmt first)  | `act --container-name-prefix={PREFIX}- -W .github/workflows/check.yml -j clippy`         |
| hack                | `act --container-name-prefix={PREFIX}- -W .github/workflows/check.yml -j hack`           |
| msrv                | `act --container-name-prefix={PREFIX}- -W .github/workflows/check.yml -j discover-msrv -j msrv` |
| required            | `act --container-name-prefix={PREFIX}- -W .github/workflows/test.yml -j required`        |
| coverage            | `act --container-name-prefix={PREFIX}- -W .github/workflows/test.yml -j coverage`        |
| sanitizers          | `act --container-name-prefix={PREFIX}- -W .github/workflows/safety.yml -j sanitizers`    |
| nightly             | `act --container-name-prefix={PREFIX}- -W .github/workflows/scheduled.yml -j nightly`    |
| update              | `act --container-name-prefix={PREFIX}- -W .github/workflows/scheduled.yml -j update`     |
| conformu            | `act --container-name-prefix={PREFIX}- -W .github/workflows/conformu.yml -j discover -j conformu` |

**Optional** — only when `$ARGUMENTS` contains `miri`:

| Agent label         | Command                                                                              |
|---------------------|--------------------------------------------------------------------------------------|
| miri                | `act --container-name-prefix={PREFIX}- -W .github/workflows/scheduled.yml -j discover-miri -j miri` |

**Notes:**
- Skip `test.yml` `os-check` job entirely (act runs Linux Docker containers).
- For `conformu.yml`, act will run the ubuntu variant only.

### 3. Subagent prompt template

Use this prompt for every Task subagent (fill in `{COMMAND}` and `{LABEL}`):

```
Run the following CI job and report the result:

  {COMMAND}

Job label: {LABEL}

INSTRUCTIONS:
1. Run the command above. Use a timeout of 600000ms.
2. Your FINAL message must be a concise synopsis in EXACTLY this format.
   Keep it short — the main agent will read this from your output file and
   must be able to parse it quickly without context overflow.

   For a PASSING job, your final message must be exactly:
   ---SYNOPSIS---
   RESULT: PASS
   JOB: {LABEL}
   ---END---

   For a FAILING job, investigate the failure FIRST (read output, identify
   root cause, read relevant source files), then write your final message:
   ---SYNOPSIS---
   RESULT: FAIL
   JOB: {LABEL}
   CATEGORY: <CODE|WORKFLOW|FLAKY>
   ERROR_SUMMARY: <one sentence — what broke and where>
   ---END---
   ---DIAGNOSIS---
   FILES_AFFECTED: <comma-separated list of file paths>
   ROOT_CAUSE: <2-3 sentences explaining why it failed>
   SUGGESTED_FIX: <specific, actionable fix — include exact code changes
     as a compact before/after snippet when possible, max ~20 lines>
   ---END---

   Failure categories:
   - CODE: compiler error, test failure, clippy lint, formatting issue
   - WORKFLOW: act compatibility issue, missing env var, GitHub-only action
   - FLAKY: timeout, race condition, intermittent failure

IMPORTANT: Do NOT edit or fix any files. You are a read-only investigator.
Only run the act command and read files to diagnose. Return your synopsis.
```

### 4. Collect results and print summary

Each background agent wrote its result to an output file (the path was returned
when you launched it). For each agent:

1. **Read the output file** using the Read tool.
2. **Extract only the `---SYNOPSIS---` block** — look for the text between
   `---SYNOPSIS---` and the first `---END---`. This is a few lines at most.
3. Parse the RESULT and JOB fields from each synopsis.

Print a summary table:

```
| Job          | Result | Category | Error Summary          |
|--------------|--------|----------|------------------------|
| clippy       | FAIL   | CODE     | unused import in foo.rs|
| hack         | PASS   | —        | —                      |
| ...          | ...    | ...      | ...                    |

Total: X passed, Y failed, Z skipped
```

### 5. Evaluate failures and fix

If **all jobs passed**, congratulate the user and stop here.

If **any jobs failed**, apply fixes **yourself** (the main agent):

1. **Read the full diagnosis** — For each failed job, go back to its output
   file and extract the `---DIAGNOSIS---` block (between `---DIAGNOSIS---` and
   `---END---`). Only read diagnosis blocks for failed jobs — do NOT re-read
   output files for jobs that passed.

2. **Review all failure diagnoses together** — Look for overlapping fixes.
   Multiple jobs may fail for the same root cause. Group related failures
   and fix the root cause once rather than applying conflicting patches.

3. **Apply fixes** — Make the minimal changes needed. Follow CLAUDE.md rules:
   - Do NOT over-engineer — fix only what is broken
   - Run `cargo fmt` after any Rust source changes
   - Run `cargo build --all --quiet --color never` to verify compilation
   - Run `cargo test --all --quiet --color never` to verify tests pass locally

4. **Re-run failed jobs only** — Spawn new background Task subagents for just
   the previously failed jobs (using the same prompt template and
   `run_in_background: true`). Do NOT re-run jobs that already passed.

5. **Repeat** — If re-runs still fail, review the new reports and try again.
   After **3 unsuccessful fix attempts** for the same job, stop and report
   the issue to the user with your diagnosis and what you've tried.

6. **Final summary** — Once all jobs pass (or max retries reached), print an
   updated summary table showing the final status of every job.

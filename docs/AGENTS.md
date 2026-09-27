# Agents and human operators MUST follow the rules below

1. You MUST read the relevant documentation before starting work:
   a. Read the design document for any service you are modifying (docs/services/<service>.md). For instance, filemonitor's design document is located in docs/services/filemonitor.md.
   b. Read the skill document for the type of task you are performing (docs/skills/). Specifically:
      - Developing a new feature or service: read docs/skills/development-workflow.md
      - Scaffolding a new long-running service binary, or touching a service's main.rs / shutdown handling: also read docs/skills/service-lifecycle.md
      - Writing or modifying tests: read docs/skills/testing.md
      - Pushing code or running CI checks: read docs/skills/pre-push.md
      - Checking code coverage, or diagnosing a red coverage check: read docs/skills/coverage.md
      - Shepherding an open pull request through CI and code review to merge readiness: read docs/skills/babysitting-prs.md
      - Running ConformU against a physical device, or adding a record to docs/validation/: read docs/skills/hardware-validation.md
      - Archiving a completed plan (moving docs/plans/<plan>.md into docs/plans/archive/): read docs/skills/archiving-plans.md

2. You MUST ALWAYS update the appropriate README and / or the appropriate design document when you make a change to a service and the change is impacting what is stated in these documents. If in doubt, re-read the docs to evaluate impact.

3. You MUST use `cargo run` when you start any service for testing.

4. You MUST ALWAYS run the local quality gate before committing your work and fix all errors and warnings from the change you've made:

   ```sh
   bazel build //... && bazel test //...                     # build + tests incl. BDD (the per-PR Bazel gate, run locally)
   cargo fmt
   cargo clippy --all-targets --all-features -- -D warnings  # Bazel runs neither rustfmt nor clippy
   cargo clippy --workspace --lib --bins -- -D warnings      # default-features pass — lints what --all-features cfgs out (#988)
   ```

   **cargo-rail is retired** — do **not** run `cargo rail`. Cargo.toml /
   Cargo.lock remain the single source of truth for dependency versions
   (see rule 10).

   Read [docs/skills/pre-push.md](skills/pre-push.md) before pushing: it
   owns the full pre-push set, which checks are required, what runs
   nightly rather than per-PR, the Bazel caching and `--config=ci`
   warning behaviour, and how to reproduce the nightly Cargo safety net
   locally via `act`.

5. You MUST NEVER commit to the main branch of the git repository. ALL work MUST happen on a branch. Before making any code changes, verify you are on a feature branch. If on main, create and switch to an appropriate feature branch first. Use appropriate naming for branches such as `feature/new_feature_name` or `chore/update_dependency_x`.

6. You MUST commit changes summarizing all the changes since the last commit. For the author of the commit, use the configured username in git with ' ($AI_AGENT_NAME)' appended and the user email. For example, `git commit --author="John Doe (Kiro CLI) <john@email.com>"` if you are Kiro or `git commit --author="John Doe (Claude Code) <john@email.com>"` if you are claude code.

7. When working on unit tests, you SHOULD prefer tests that will fail with clear errors (e.g. use `result.unwrap()`, instead of `assert!(result.is_ok())`). See docs/skills/testing.md for the complete testing guide.

8. You SHOULD use test that test the smallest amount of functionality possible, while still being comprehensive in aggregate.

9. You MUST use `debug!()` log messages throughout. Only use `info!()` log messages where users will derive clear advantage from them when using the services, such as `Service started succesfully`.

10. You MUST add dependencies to the workspace Cargo.toml when more than one service has the same dependency. Cargo.toml and Cargo.lock remain the single source of truth for dependency versions; Bazel's `crate_universe` reads them. After adding a crates.io dependency you MUST refresh `MODULE.bazel.lock` before committing — the exact three-command recipe, and why each command is needed, is in [docs/skills/pre-push.md](skills/pre-push.md).

11. You MUST persist project-wide knowledge (design decisions, motivations, conventions) in the repository documentation (docs/, README.md, ADRs) rather than in local agent memory. This ensures all operators and machines share the same context.

12. Investigations on `main` MUST be read-only. To inspect old code or compare states, use commands that write to stdout (`git show <ref>:<path>`, `git diff <ref>`, `git log -p`, `git cat-file -p`) — never commands that mutate the working tree or index (`git checkout <ref> -- <path>`, `git restore --source`, `git apply`, `git stash pop`). If you genuinely need to materialize an old state (e.g. to run tests against it), do it in a throwaway worktree (`git worktree add`), not on `main`. Before declaring any investigation complete, run `git status`; if it isn't clean, surface the diff to the user rather than silently leaving staged or modified files behind.

13. Code you write or review MUST honor the project tenets in [docs/workspace.md](workspace.md#project-tenets) — in particular tenet 3, *no actuation on connect*: no code path reachable from service startup, driver connect/reconnect, config apply, or a passive/supervisory transition may physically actuate hardware (motion, homing, park slews, cover/lamp, cooler setpoints, power or dew toggles, filter moves, guide pulses). Stop-class commands (halt/abort) and cleanup inside an operator-started session are permitted. When reviewing a change to a connect path, handshake hook, or supervisory loop, check this explicitly.

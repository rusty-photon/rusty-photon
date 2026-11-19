# Agents and human operators MUST follow the rules below

1. You MUST read the design documentation before you start working on a task. The design doc is [qhyccd-rs.md](qhyccd-rs.md), next to this file.

2. You MUST ALWAYS update the appropriate README and / or the design document when you make a change and the change is impacting what is stated in these documents. If in doubt, re-read the docs to evaluate impact.

3. You MUST use `cargo run` when you start any service for testing.

4. You MUST ALWAYS run `bazel build //...`, `bazel test //...` and `cargo fmt` to build the package before committing your work and fix all errors and warnings from the change you've made.

5. You MUST NEVER commit to the main branch of the git repository. ALL work MUST happen on a branch. Use appropriate naming for branches such as `feature/new_feature_name` or `chore/update_dependency_x`.

6. You MUST commit changes summarizing all the changes since the last commit. For the author of the commit, use the configured username in git with ' ($AI_AGENT_NAME)' appended and the user email. For example, `git commit --author="John Doe (Kiro CLI) <john@email.com>"` if you are Kiro or `git commit --author="John Doe (Claude Code) <john@email.com>"` if you are claude code.

7. When working on unit tests, you SHOULD prefer tests that will fail with clear errors (e.g. use `result.unwrap()`, instead of `assert!(result.is_ok())`).

8. You SHOULD use tests that test the smallest amount of functionality possible, while still being comprehensive in aggregate.

9. You MUST use `debug!()` log messages throughout. Only use `info!()` log messages where users will derive clear advantage from them when using the services, such as `Service started successfully`.

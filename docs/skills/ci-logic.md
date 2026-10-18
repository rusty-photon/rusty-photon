# Skill: Writing CI logic

[ADR-022](../decisions/022-ci-logic-in-javascript-actions-and-a-rust-tools-crate.md)
decides where CI logic lives: workflow YAML for a step that runs one
command, a local JavaScript action for logic that runs inside GitHub
Actions, one Rust tools crate for tools a developer also runs. This doc
says how to write it there. Moving the existing shell is
[docs/plans/ci-shell-to-code.md](../plans/ci-shell-to-code.md).

## When to Read This

- Before adding or changing a workflow step, or a composite action's step,
  that runs more than a few commands or has any control flow
- Before writing or changing a local action under `.github/actions/`
- Before picking up an item of the CI shell plan

## Where it goes

| The logic | Its home |
| --- | --- |
| One command (a `bazel`, `cargo` or similar invocation, with its flags), or a short fixed sequence of them with no control flow | The workflow YAML, as a `run:` step |
| Anything else that only CI runs: installing tools, choosing flags, caches, the GitHub API, reading `cargo metadata` or `bazel query` output | A local JavaScript action |
| A tool a developer also runs (AGENTS.md rules 4 and 10 name some) | The Rust tools crate, `tools/rusty-photon-ci`, run as `cargo ci <tool>` |

Shell stays for the one-command steps above and for what ADR-022 leaves
out: host provisioning, the Python scripts, the git hooks. A reviewer may
ask for a new shell block of more than a few lines to move.

## The Rust tools crate

`tools/rusty-photon-ci` is a workspace member like any other: it builds
under Cargo and Bazel, under the workspace lints and MSRV, with
`publish = false`. A developer runs a tool with `cargo ci <tool>`; the
alias is in `.cargo/config.toml`, and `cargo ci help` lists the tools.

```
tools/rusty-photon-ci/
  Cargo.toml
  BUILD.bazel              the library, the binary and the unit test
  src/main.rs              clap: one subcommand per tool, nothing else
  src/lib.rs
  src/process.rs           Cmd, the Runner trait, and System, which runs for real
  src/<tool>.rs            one module per tool, with its tests
  src/pool_routing.rs      a guard over the workflows, compiled only for tests
```

- **One module per tool, one subcommand per module.** `main.rs` parses
  the command line, calls the tool and prints what it returns; it holds no
  logic, since no test runs it.
- **A tool takes a `Runner`**, not `std::process::Command`. It builds each
  command as a `Cmd` (program, arguments, directory, the variables it sets
  and removes) and hands it to `run` or `read`. Its tests pass a fake that
  records each `Cmd` and answers `read` from a queue, so they check what
  the tool would run, in what order and with what environment, and what it
  does when a command fails, with no Bazel or git on the test's host.
  `process.rs`'s own tests run `System` on the test binary itself, the one
  program every OS and Bazel's sandbox are sure to have.
- **A guard over the repository's own files is a module only the tests
  compile** (`#[cfg(test)] mod` in `lib.rs`), with no subcommand:
  `bazel test //...` runs it on every PR. The files it reads are the unit
  test's `data` in `BUILD.bazel`. It finds them from `CARGO_MANIFEST_DIR`:
  Cargo sets it when it runs a test, and the BUILD file sets it for Bazel,
  relative to the runfiles root. It reads a YAML file with the parser
  (`saphyr`), never line by line: R6's guard started line by line, and
  each review of it found YAML it misread.
- **The shell it replaces goes in the same PR**, with every mention of it
  (ADR-022): search the repository for its path before deleting it.
- **Dependencies are the workspace's** (rule 10), and few: `clap` for the
  command line, `thiserror` for the errors, and for the tests `saphyr`, a
  YAML parser that keeps each node's line.
- **CI runs the crate's tests, not its tools, yet.** The Linux pool
  template has no Cargo until the plan's T1, so a workflow step that does
  what a tool does stays YAML, and the tool's source names the step to
  keep in step with.

## JavaScript actions

### Layout

```
.github/actions/
  BUILD.bazel              the tests of every action, and the link check
  node_test.bzl            node_test and node_link_test
  link.test.mjs            parses and links every JavaScript file here
  lib/helpers.mjs          the helper module every action imports
  lib/helpers.test.mjs
  lib/testing.mjs          test fixtures every action's tests share
  <action>/action.yml      runs.using: node24, runs.main: index.mjs
  <action>/index.mjs       the entry file: inputs in, outputs out
  <action>/<logic>.mjs     what the tests import
  <action>/<logic>.test.mjs
```

- **ES modules, as `.mjs`.** The runner starts an action as
  `node "<main>"` (actions/runner, `NodeScriptActionHandler.cs`), and Node
  reads a `.mjs` file as an ES module with no `package.json` to say so.
  An entry file can use top-level `await`.
- **Node built-ins only**: `node:` imports and the globals (`fetch`,
  `AbortSignal`). No npm package, `package.json`, `node_modules`, bundler
  or TypeScript.
- **Keep the entry file thin.** It runs on load, so no test imports it.
  It reads the inputs, calls the logic in a module beside it, and writes
  the outputs, all inside `run`:

  ```js
  import { getInput, run, setOutput } from '../lib/helpers.mjs';
  import { cacheKey } from './key.mjs';

  await run(async () => {
    setOutput('key', cacheKey(getInput('pin', { required: true })));
  });
  ```

- **Node versions.** Actions run on the runner's own Node 24.
  `MODULE.bazel` pins a Node 24 release for the tests, so code that works
  in the tests works there.

### The helper module

`.github/actions/lib/helpers.mjs` is what `@actions/core` and
`@actions/http-client` would provide. Its formats follow the runner's
parsers, which its comments name.

| Export | Use |
| --- | --- |
| `getInput`, `getBooleanInput` | Read an input. A boolean accepts `true`/`True`/`TRUE` and the three `false` spellings; anything else, empty included, throws, so give it a default in `action.yml`. |
| `setOutput`, `exportVariable`, `addPath` | Write `GITHUB_OUTPUT`, `GITHUB_ENV`, `GITHUB_PATH`. Values always go in the multi-line form, under a random delimiter. A name the runner would split (`=`, `<<`, a line break) throws, and so does `NODE_OPTIONS`, which the runner refuses to pass to later steps. |
| `setSecret` | Mask a value in the rest of the job's log. Call it before anything can print the value. |
| `saveState`, `getState` | Keep a value for the action's own `post:` step, which reads it back; the runner hands it to that step alone. |
| `warning`, `group`, `fail` | A warning annotation; a collapsible log group that closes however its function ends; an error annotation that fails the step on exit. |
| `run` | Every entry file's wrapper: a throw becomes an error annotation and a failed step, with the stack in the log. |
| `describe` | An error's message followed by its causes', which is where `fetch` says why a request failed (`fetch failed: connect ECONNREFUSED …`): what `run` puts in its annotation, for an action that reports a failure without failing the step. |
| `fetchWithRetry` | `fetch` with a timeout per attempt and bounded retries of 408, 429, 500, 502, 503, 504, a 403 that says how long to wait (how GitHub's API answers a rate limit), timeouts and requests that got no answer, waiting as long as a Retry-After or the GitHub API's rate-limit reset asks, or a minute for its secondary limit, measured on the server's clock (a wait over two minutes is not retried). Each retry is a plain log line, not a warning annotation. Anything else outside 2xx throws an `HttpError`. Only idempotent methods are retried by default: a POST or PATCH gets one attempt unless the caller sets `attempts`, knowing a repeat is harmless. The timeout covers reading the body too, so size it for the download. A stream body needs `attempts: 1`, since a retry would send it again. A caller's own `signal` aborts the request and ends the retries. A URL with a user or password is refused, as is one that does not parse, and neither error shows the URL. |
| `HttpError` | Its message holds the method, the URL without its query string (where a pre-signed URL carries its signature) and the status. `body` holds the start of the response's body, where an API says why it refused. That stays out of the message and the logged stack, since a server may echo credentials there; add it to an error of your own when you trust the server. |
| `retry` | The same bounded retry for any other step that can fail transiently, such as a download and its checksum as one unit. Give a `fetchWithRetry` inside it `attempts: 1`, or the two retry counts multiply (3 × 3 requests). `attempts` must be a whole number of 1 or more. The default `isRetryable` is `isTransient`, which retries no bug in the callback. A Retry-After over two minutes is never waited out, whatever `isRetryable` says. An abort during a wait throws an AbortError whose cause is the error being retried. |
| `maskUrl` | Mask a URL that is the operator's own, such as the LAN cache's: the URL, its host and its hostname. Returns it without trailing slashes; one that does not parse throws without showing it. |
| `requestFailure` | Why a request failed, without the address it went to: the HTTP status, or the error codes along its causes. `fetch`'s own messages name the address it connected to, which no mask of a host name covers. |
| `sha256` | The hash of a stream: a file's (`fs.createReadStream`) or a response body's. |
| `io` | Where the helpers write. Tests swap it out (below). |

A helper goes into `lib/helpers.mjs` once two actions need it; until then
it stays in the action.

### Tests

- Test files sit beside the code as `<name>.test.mjs`, written with
  `node:test` and `node:assert/strict`.
- Declare each in `.github/actions/BUILD.bazel`:

  ```starlark
  node_test(
      name = "install_omnisim_test",
      size = "small",
      srcs = ["install-omnisim/plan.test.mjs"],
      data = [
          "install-omnisim/plan.mjs",
          "lib/helpers.mjs",
      ],
  )
  ```

  `data` lists every file the test imports or reads. The launcher starts
  Node with `--preserve-symlinks`, so an import missing from `data` fails
  on Linux the way it would on Windows, where the runfiles are copies.
- **`link_test` checks every file.** It parses each one as an ES module
  and links it without running it: every static import must be a `node:`
  built-in or a relative `.mjs` file that exists, and every name imported
  must be exported there. So a misspelt import in an entry file, which no
  test imports, fails the PR instead of the runner, and so does an npm
  package, a `.js` file or TypeScript.
- **What `link_test` cannot catch.** It cannot follow a dynamic
  `import()`, so actions use static imports only. Nor does it catch a
  name a file uses without declaring or importing it (a misspelt call):
  that shows only when the code runs. That is one more reason to keep an
  entry file to a few lines and the logic in a module its tests import.
- **No BUILD file below `.github/actions`.** `link_test` checks the files
  its glob sees, and a glob stops at a package boundary, so
  `node_link_test` refuses a nested BUILD file rather than lose sight of
  the files under it.
- `bazel test //.github/actions/...` runs them. They are part of
  `bazel test //...`, so they run on the Linux and Windows PR legs, on
  macOS on main, and in the coverage job.
- **Read what an action prints through `io`**, never by replacing
  `process.stdout.write`. Under `node --test`, stdout also carries the
  test runner's reports to its parent process. Replacing it loses them,
  and the file fails with only some of its tests reported. `capture` in
  `lib/testing.mjs` does it; list that file in the test's `data`.
- **Fakes.** A fake GitHub API, store or download server is a `node:http`
  server on `127.0.0.1`, port 0: `serve` in `lib/testing.mjs`, which
  records each request with its body. Command
  files are temp files under `TEST_TMPDIR`. An environment variable a test
  sets is put back when it ends (`withEnv` in `lib/testing.mjs`).
- Show that a test which guards a behaviour fails with the behaviour
  broken: break it on purpose and watch the test go red.
- There is no JavaScript formatter or linter (ADR-022). Match
  `helpers.mjs`: two-space indent, single quotes, semicolons.

### An action with a post step

An action that also has work to do when the job ends, such as saving a
cache, declares `runs.post` beside `runs.main`, with its own entry file.
It runs at the end of the job, and by default after a failed or cancelled
job too (`post-if` "defaults to `always()`", and its status functions read
the job's status): give `runs.post-if: success()` to a post step that must
not act on a half-finished job. The
two entry files run as separate processes; what the post step needs from
the main step goes through `saveState` and `getState`, and its inputs are
the main step's. `cargo-cache` is the example.

### A job that checks nothing else out

A local action is read from the checkout, so a job that needs nothing
else from the repository, such as a nightly's tracking-issue job, checks
out only the action's directory and `lib`, without keeping the token, and
declares `contents: read` beside what the action needs:

```yaml
    permissions:
      contents: read
      issues: write
    steps:
      - uses: actions/checkout@v7
        with:
          persist-credentials: false
          sparse-checkout: |
            .github/actions/tracking-issue
            .github/actions/lib
      - uses: ./.github/actions/tracking-issue
```

### An action that needs another action

A JavaScript action cannot call another action. One that needs
`actions/cache` is a composite action with three steps, in the order
ADR-022 gives:

1. a JavaScript action that resolves what to fetch and outputs the cache
   key, which carries the pin;
2. `actions/cache` with that key;
3. a JavaScript action that reads the cache's `cache-hit` output and
   installs.

The composite calls each JavaScript step with `uses:` on its own directory
(`uses: ./.github/actions/<action>/resolve`), never as `run: node …`. A
`run:` step gets whatever `node` the runner's PATH has, which may be none
or another version; only a JavaScript action runs on the runner's own
Node. A `./` path resolves against the checkout, so the workflow checks the
repository out first, as every workflow that uses a local action already
does.

### Proving it

A PR's CI only runs the workflows its events trigger. Name every runner
the code runs on (hosted Linux, Windows and macOS, the pool's Linux and
Windows templates, the Pi) and show each one green, or say what stays
unproven until it first runs on main and watch that run. The plan's
[Working an item](../plans/ci-shell-to-code.md#working-an-item) covers the
workflows that cannot be dispatched on a branch.

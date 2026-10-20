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
| A developer's install of what an `install-*` action installs | That action's own module, which `bazel run //.github/actions:install` runs ([below](#a-developers-install)) |

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
  YAML parser that keeps each node's line, with `saphyr-parser`, whose
  events keep every tag.
- **CI runs the crate's tests, not its tools, yet.** The Linux pool
  template has no Cargo until the plan's T1, so a workflow step that does
  what a tool does stays YAML, and the tool's source names the step to
  keep in step with.

## JavaScript actions

### Layout

```
.github/actions/
  BUILD.bazel              the tests of every action, and the link check
  node_test.bzl            node_test, node_link_test and node_binary
  link.test.mjs            parses and links every JavaScript file here
  lib/helpers.mjs          the helper module every action imports
  lib/helpers.test.mjs
  lib/github.mjs           the GitHub REST API, for the actions that call it
  lib/install.mjs          download, check and unpack, for the install-* actions
  lib/install.test.mjs
  lib/testing.mjs          test fixtures every action's tests share
  local-install/           a developer's install, by the install-* modules
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
| `warning`, `error`, `group`, `fail` | A warning annotation; an error annotation that leaves the step to succeed, for a failure a later step acts on; a collapsible log group that closes however its function ends; an error annotation that fails the step on exit. |
| `run` | Every entry file's wrapper: a throw becomes an error annotation and a failed step, with the stack in the log. |
| `describe` | An error's message followed by its causes', which is where `fetch` says why a request failed (`fetch failed: connect ECONNREFUSED …`): what `run` puts in its annotation, for an action that reports a failure without failing the step. |
| `fetchWithRetry` | `fetch` with a timeout per attempt and bounded retries of 408, 429, 500, 502, 503, 504, a 403 that says how long to wait (how GitHub's API answers a rate limit), timeouts and requests that got no answer, waiting as long as a Retry-After or the GitHub API's rate-limit reset asks, or a minute for its secondary limit, measured on the server's clock (a wait over two minutes is not retried). Each retry is a plain log line, not a warning annotation. Anything else outside 2xx throws an `HttpError`. Only idempotent methods are retried by default: a POST or PATCH gets one attempt unless the caller sets `attempts`, knowing a repeat is harmless. The timeout covers reading the body too, so size it for the download. A stream body needs `attempts: 1`, since a retry would send it again. A caller's own `signal` aborts the request and ends the retries. A URL with a user or password is refused, as is one that does not parse, and neither error shows the URL. |
| `HttpError` | Its message holds the method, the URL without its query string (where a pre-signed URL carries its signature) and the status. `body` holds the start of the response's body, where an API says why it refused. That stays out of the message and the logged stack, since a server may echo credentials there; add it to an error of your own when you trust the server. |
| `retry` | The same bounded retry for any other step that can fail transiently, such as a download and its checksum as one unit. Give a `fetchWithRetry` inside it `attempts: 1`, or the two retry counts multiply (3 × 3 requests). `attempts` must be a whole number of 1 or more. The default `isRetryable` is `isTransient`, which retries no bug in the callback. A Retry-After over two minutes is never waited out, whatever `isRetryable` says. An abort during a wait throws an AbortError whose cause is the error being retried. |
| `maskUrl` | Mask a URL that is the operator's own, such as the LAN cache's: the URL, its host and its hostname. Returns it without trailing slashes; one that does not parse throws without showing it. |
| `requestFailure` | Why a request failed, without the address it went to: the HTTP status, or the error codes along its causes. `fetch`'s own messages name the address it connected to, which no mask of a host name covers. |
| `command` | Runs a program with its arguments, never through a shell, and resolves to what it prints. An exit other than 0 rejects with the last line of its stderr, and a program that cannot start with its spawn error (`ENOENT`). |
| `sha256` | The hash of a stream: a file's (`fs.createReadStream`) or a response body's. |
| `parseRetryAfter` | The wait a Retry-After header asks for, in ms: its seconds, or its HTTP date measured against the server's clock. For a request made without `fetchWithRetry`, as `lib/install.mjs`'s download is. |
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
      srcs = ["install-omnisim/omnisim.test.mjs"],
      data = [
          "install-omnisim/omnisim.mjs",
          "lib/helpers.mjs",
          "lib/install.mjs",
          "lib/testing.mjs",
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
  sets is put back when it ends (`withEnv` in `lib/testing.mjs`). A tar
  archive is built in memory (`tar` in `lib/testing.mjs`), so a test needs
  no system `tar` to make one and can make one no packager would.
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

### An action that installs a tool

`lib/install.mjs` holds what every install action does, and
`install-omnisim` is the example: `omnisim.mjs` keeps the release, the
asset table and its pins, `resolve/` and `install/` are steps 1 and 3
above.

- **A table keyed by runner**, `${RUNNER_OS}-${RUNNER_ARCH}`
  (`runnerKey`), read with `forRunner`, which names the runners the table
  has when it lacks this one.
- **Every asset carries a pin**, of one of three kinds: `{ sha256 }`,
  checked against the download; `{ ref }`, a git commit's full SHA that
  the URL names, for the blobs the INDI repository vendors, which have no
  hash of their own; `{ rolling: true }`, for a vendor's "latest" link,
  which has none. `cacheKey` carries each asset's name and pin, and a
  rolling pin's ISO week, so moving a pin misses the old cache and a
  rolling download is fetched again each week. A run of assets that share
  a pin, such as the files of one commit, carries it once.
- **`download`** retries a failed request, a body cut short, a transfer
  that stalls (no data for 30 s) and a SHA-256 mismatch, as one unit with
  its check, and leaves no file unless every check passed. It fetches with
  Node's own HTTP client (`get`), not `fetch`: Node 24's `fetch` (undici 7)
  crashes the process, past any `catch`, when a server closes the
  connection while the reader has paused the body
  ([nodejs/undici#5360](https://github.com/nodejs/undici/issues/5360),
  fixed in undici 8 only). A SourceForge mirror answers
  `Connection: close`, and two of three legs of a `plate-solver-smoke` run
  crashed so. `get` follows redirects, refuses one from https to http, and
  throws an `HttpError` for an answer outside 2xx. For a server
  that crawls rather than fails, such as a SourceForge mirror, a caller
  can also set a floor on the rate (`minBytesPerSecond`, measured over the
  stall time) and a time after which no attempt starts (`retryForMs`), as
  `install-astap` does for its 100 MB database. A failure a later attempt
  would meet again (a 404, a bad certificate) is final, unless the caller
  sets `retryAll` for a pool of mirrors, where the next attempt likely
  reaches another; `install-astap` does, as SourceForge's are one.
- **`downloadAll`** downloads an action's assets into the directory its
  cache keeps, all of them or none, unless the cache restored them all.
  It holds a rolling pin's fallback. The action restores with `restoreKey`
  as `restore-keys`, so a week whose key misses restores the newest
  earlier week's download; when the vendor then fails, `downloadAll` warns
  and installs that one. Such an action restores and saves in two steps
  (`actions/cache/restore`, `actions/cache/save`), and saves only a new
  download, which its install step's output names: an earlier week's
  saved under this week's key would stop the weekly refresh.
  `install-zwo-sdk` is the example, for ZWO's SDKs for Windows.
- **`makeRunnable`** sets a binary's mode so it runs, and leaves a
  Windows `.exe` as it is.
- **A private download sends its token as `headers`** to `download`,
  which `get` sends on through a redirect to the same origin only, as
  fetch does. `install-svbony-sdk` does, for the SVBony SDK for Windows on
  the project's private mirror (ADR-018 §7), and caches nothing it
  fetched with the token: a fork's pull request can restore the
  repository's caches.
- **An SDK a native crate links** has the helpers both SDK actions share:
  `findRuntime` finds a runtime library through the loader's cache,
  `linkRuntime` links a -dev package's linker name to it on the sudo-less
  Pi, `loaderFinds` says whether the loader's cache lists a library where
  it went, `canWrite` whether the user can write a prefix without `sudo`,
  and `unpackZips` unpacks a vendor's zip on Windows with PowerShell's
  `Expand-Archive`, since Windows' `tar.exe` fails on a name the code
  page cannot hold, as ZWO's SDKs have.
- **`unpack`** runs each OS's own tool, since Node's built-ins read no
  archive format and have no xz decoder: on Linux GNU tar, and `unzip`
  for a zip, which GNU tar does not read; on macOS and Windows bsdtar,
  which reads both. Windows' is `%SystemRoot%\System32\tar.exe`, run by
  that path: Git for Windows' GNU tar can come first on PATH. A zip on
  Linux takes `unzip` rather than a zip reader of our own, which is what
  the shell ran before. `onlyDirectory` finds the folder an archive
  unpacked into.
- **Each OS leg tests its own tools** on the formats its assets come in:
  `install_test` unpacks small archives made by GNU tar, xz and Info-ZIP's
  zip.
- **The release lives in the module**, beside the pins it is pinned to,
  not in an `action.yml` input: a new version needs new pins, and the
  developer's install below reads the same constants.
- **A table that a workflow rewrites is JSON**, in a file beside the
  module that reads it: `install-astap`'s `assets.json`, whose pins
  `refresh-astap` compares with what upstream serves and rewrites in a
  pull request. It reads and writes the table whole, as JSON, so it does
  not depend on how the file is laid out, and a test checks the file
  keeps the one layout the rewrite writes, so a refresh's diff is the
  pins alone.

### A developer's install

`bazel run //.github/actions:install -- <tool> [--prefix DIR]` installs a
tool an `install-*` action installs, from the same release and URL,
checked against the same pin, by that action's own module. It runs on the
Node `MODULE.bazel` pins, through `node_binary`; ADR-022 makes this the
exception to "a tool a developer also runs is Rust", since a Rust tool
would be a second implementation of the module to keep in step.

- **Each tool's module exports `installAt(dir, resolved, options)`**
  beside `install`: it installs what `resolve` chose into a directory the
  developer names and resolves to the variables that locate it, where
  `install` exports them to the job. `local-install/local.mjs` lists the
  tools in `TOOLS`.
- **It never removes what it did not install.** A tool that installs a
  directory whole marks it (`markInstalled`), and replaces a directory
  only when it is empty or holds that mark (`checkReplaceable`); one that
  installs single files replaces only those. Its scratch directory goes
  however the install ends, Ctrl-C included (`withWorkDir`).
- **Where it installs.** `--prefix` names the directory, and
  `~/.local/opt/<tool>` is the default, unless the tool's `TOOLS` entry
  gives a `dir` of its own, for a build that looks in one place only. The
  ZWO SDK's is `/usr/local` on Linux and macOS, where libzwo-sys's build
  script and Bazel's actions find it, as CI installs it; it copies the
  files there with `sudo` when the developer cannot write there. The
  SVBony SDK's is `/usr/local` too, where libsvbony-sys's build script
  looks.
- **It ships nothing.** It downloads on the developer's machine from
  where CI downloads, as the operator helpers of ADR-013 and ADR-018 do:
  nothing goes into a package, and nothing is mirrored for developers.
  Nothing downloads from the project's private mirror (ADR-018 §7): the
  SVBony SDK installs on Linux and Intel Macs, and on Windows, whose SDK
  only the mirror holds, says where to get it.
- **Its launcher is tested as `bazel run` runs it**, from the runfiles
  tree: `local_install_test` takes `:install` as data and runs it, so a
  launcher that drops its arguments or its exit code fails the PR. The
  test lists nothing else `:install` brings, so its imports come from
  `:install`'s runfiles, and a module or table missing from them fails
  it too.

### An action that runs processes

`bdd-sharded` is the example: it starts the BDD suites as processes at
once and waits for them all.

- **`spawn` a program, never through a shell**, with stdin `'ignore'`:
  a program that read stdin would take the step's, and in a list run one
  after another, the next one's. `command` in the helper module does this
  for one program whose output the action reads.
- **Programs that run at once write to a file each**: pass the file's
  descriptor as the child's stdout and stderr, and print each file
  afterwards in a group of its own. Written to the step's log as they
  came, their lines would interleave.
- **A stopped step.** The runner stops a cancelled step, one past its
  job's `timeout-minutes` included, with SIGINT, then SIGTERM 7.5 s
  later, then kills it 2.5 s after that (actions/runner,
  `src/Runner.Sdk/ProcessInvoker.cs`; Ctrl+C and Ctrl+Break on Windows).
  On Linux and macOS the kill reaches Node alone, and the processes it
  started go on. So the entry file listens for both signals and aborts an
  `AbortController` the logic takes as `signal`; the logic stops its
  children and prints what it must within the 7.5 s. `bdd-sharded` sends
  each SIGTERM, then SIGKILL 5 s later, and prints the logs.
- **A program the action downloaded and nobody has checked** gets the
  step's environment without the runner's `ACTIONS_*` variables and the
  action's `INPUT_*`. A JavaScript action gets `ACTIONS_RUNTIME_TOKEN`,
  which can write the repository's caches, and a `run:` step does not
  (actions/runner, `NodeScriptActionHandler.cs`). Its job also runs with
  `contents: read` and `persist-credentials: false`, so no token is on
  disk. `refresh-astap`'s `detect` runs a new ASTAP release so. That only
  keeps the token from being handed to the program; it is not a sandbox.
  The action's own process still holds it, and on Linux a process can
  read its parent's environment (`/proc/<ppid>/environ`), where a `run:`
  step's program would have had to dig it out of the runner's memory; a
  hosted runner's user also has passwordless `sudo`. Keep such a job
  holding nothing else.
- **Tests start real processes.** The logic takes `spawn` as an option,
  and a test passes one that starts `node -e` with a fake in each
  program's place, so the descriptors, the environment and the signals
  are real. An `'ipc'` channel added to the child's stdio lets the fake
  say when it is ready, so a test can stop it at a known point.

### An action that calls the GitHub API

`lib/github.mjs` holds what `tracking-issue` and `refresh-astap` share:
`runContext` (the API, the repository and the run's link from the
runner's variables), `github` (a request that names what failed and
carries GitHub's own answer, a POST or PATCH sent once; with `missing`, a
404 is no error), `openIssues` (every page, pull requests left out, a
next page on another origin refused) and `commentOrOpen` (the open issue
with a label and an exact title gets a comment, or one is opened; an
issue opened without the label fails the step, since GitHub drops a label
the repository lacks and says nothing).

- **The token is an input** (`default: ${{ github.token }}`), and the
  job grants only what the action needs, beside `contents: read`.
- **A change to the repository goes through the API**, not a push from a
  checkout: `refresh-astap` reads the file at main's tip
  (`git/ref`, `git/commits`, `contents`), writes a tree on main's and a
  commit on main (`git/trees`, `git/commits`), and creates or moves its
  branch to it (`git/refs`). Its job checks out the action only, with no
  token left on disk, and holds no clone to push from.
- **Tests run against a fake API** (`serve`) that answers in the shapes
  the real one gives; record a real answer to each read with
  `gh api` and take the writes' from GitHub's REST docs.

### Proving it

A PR's CI only runs the workflows its events trigger. Name every runner
the code runs on (hosted Linux, Windows and macOS, the pool's Linux and
Windows templates, the Pi) and show each one green, or say what stays
unproven until it first runs on main and watch that run. The plan's
[Working an item](../plans/ci-shell-to-code.md#working-an-item) covers the
workflows that cannot be dispatched on a branch.

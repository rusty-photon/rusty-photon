# Skill: Writing CI logic

[ADR-022](../decisions/022-ci-logic-in-javascript-actions-and-a-rust-tools-crate.md)
decides where CI logic lives: workflow YAML for a step that runs one
command, a local JavaScript action for logic that runs inside GitHub
Actions, one Rust tools crate for tools a developer also runs. This doc
says how to write it there. The existing shell moved under
[docs/plans/archive/ci-shell-to-code.md](../plans/archive/ci-shell-to-code.md),
now complete.

## When to Read This

- Before adding or changing a workflow step, or a composite action's step,
  that runs more than a few commands or has any control flow
- Before writing or changing a local action under `.github/actions/`

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
  src/process.rs           Cmd, the Runner trait, System, which runs for real,
                           the Clock a verifier waits with, and for the
                           tests Fake, Script and a Stopped clock
  src/<tool>.rs            one module per tool, with its tests
  src/packaging.rs         what the packaging tools share: the services, the
                           SDK pins, the ports, downloads, checksums
  src/build_packages.rs    the Linux debs and rpms, a tool CI runs
  src/verify_packages.rs   their lifecycle in a container, likewise
  src/build_tarballs.rs    the macOS tarballs, likewise
  src/verify_brew.rs       their Homebrew lifecycle, likewise
  src/build_msi.rs         the Windows suite MSI, likewise
  src/verify_msi.rs        its install-to-uninstall lifecycle, likewise
  src/brew_formulas.rs     the Homebrew tap's formulas, likewise
  src/pool_routing.rs      a guard over the workflows, compiled only for tests
  src/toolchain_pins.rs    a guard over MODULE.bazel's Rust pins, likewise
  src/pkg_assets.rs        a guard over the packaging files, likewise
  testdata/                files the tests include_str!: saved channel
                           manifests, the formulas the shell rendered
```

- **One module per tool, one subcommand per module.** `main.rs` parses
  the command line, calls the tool and prints what it returns; it holds no
  logic, since no test runs it.
- **A tool takes a `Runner`**, not `std::process::Command`. It builds each
  command as a `Cmd` (program, arguments, directory, the variables it sets
  and removes) and hands it to `run` or `read`. Its tests pass
  `process::Fake`, which records each `Cmd` and answers `read` from a
  queue, so they check what the tool would run, in what order and with what
  environment, and what it does when a command fails, with no Bazel, git,
  curl or gpg on the test's host. A download is a `read` of `curl`, so a
  test can answer it with a saved file. A tool that drives a system, as
  the packaging verifiers drive podman, Homebrew or Windows' service
  manager, is tested with `process::Script`, whose rules answer each
  command by its prefix and can read the command, its exit code included,
  so a test can stand in for the system and break it one way at a time;
  and a verifier waits through a `Clock`, which the tests' `Stopped`
  advances only when the run sleeps.
  `process.rs`'s own tests run `System` on the test binary itself, the one
  program every OS and Bazel's sandbox are sure to have.
- **A test's fixture is a file in `testdata/`** that the test
  `include_str!`s, so it needs no path at run time; the unit test's
  `compile_data` in `BUILD.bazel` gives Bazel the directory.
- **A guard over the repository's own files is a module only the tests
  compile** (`#[cfg(test)] mod` in `lib.rs`), with no subcommand:
  `bazel test //...` runs it on every PR. The files it reads are the unit
  test's `data` in `BUILD.bazel`. It finds them from `CARGO_MANIFEST_DIR`:
  Cargo sets it when it runs a test, and the BUILD file sets it for Bazel,
  relative to the runfiles root. It reads a YAML file with the parser
  (`saphyr`), never line by line: R6's guard started line by line, and
  each review of it found YAML it misread. No parser in the workspace
  reads Starlark, so the guard over `MODULE.bazel` reads its
  `rust.toolchain` call line by line, in the shape `rust-toolchain-sha256s`
  prints, and fails on any other line inside `versions` and `sha256s`
  rather than skipping it.
  A guard that reads every service's files, as the packaging guard does,
  takes the services from the workspace's members and fails on one it
  cannot read: under Bazel it sees only the files its test's `data`
  names, so a service left out of `data` would otherwise go unchecked.
- **The shell it replaces goes in the same PR**, with every mention of it
  (ADR-022): search the repository for its path before deleting it.
- **Dependencies are the workspace's** (rule 10), and few: `clap` for the
  command line, `thiserror` for the errors, `serde` with `toml` (reading
  only) for TOML, `tempfile` for a work directory, and for the tests
  `saphyr`, a YAML parser that keeps each node's line, with
  `saphyr-parser`, whose events keep every tag. A tool may run a program
  instead of taking a large crate: `rust-toolchain-sha256s` runs `gpg` to
  check a signature, where an OpenPGP crate would bring about 150 crates
  into every build of `//...` for a tool run once a Rust release.
- **A workflow runs a tool with `cargo ci <tool>`**, which builds it from
  the checkout. The packaging tools are the ones so far:
  `build-packages` and `verify-packages` in the Linux jobs,
  `build-tarballs`, `verify-brew` and `brew-formulas` in the macOS jobs,
  and `build-msi` and `verify-msi` in the Windows ones, which install the
  toolchain to build the services anyway, and
  `brew-formulas` in the nightly's `publish` job and `release.yml`'s tap
  job too, on hosted Ubuntu, after `dtolnay/rust-toolchain@stable`, as
  every job that builds Rust here runs it. A tool that builds the
  workspace runs `cargo` from inside `cargo run`, which works: Cargo holds
  no lock on the target directory once the tool is built. A step that does
  what a tool does but does not run it stays YAML, and the tool's source
  names the step to keep in step with (`repin-bazel-lock`,
  `repin-bazel.yml`). The Linux pool template has carried Cargo since the
  plan's T1, so a tool run there needs no install; a tool that drives
  Bazel settles how it is launched in the Bazel legs (ADR-022's
  Consequences).

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
  lib/processes.mjs        programs run at once, for the actions that run processes
  lib/processes.test.mjs
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
| `notice`, `warning`, `error`, `group`, `fail` | A notice annotation; a warning annotation; an error annotation that leaves the step to succeed, for a failure a later step acts on; a collapsible log group that closes however its function ends; an error annotation that fails the step on exit. |
| `run` | Every entry file's wrapper: a throw becomes an error annotation and a failed step, with the stack in the log. |
| `describe` | An error's message followed by its causes', which is where `fetch` says why a request failed (`fetch failed: connect ECONNREFUSED …`): what `run` puts in its annotation, for an action that reports a failure without failing the step. |
| `fetchWithRetry` | `fetch` with a timeout per attempt and bounded retries of 408, 429, 500, 502, 503, 504, a 403 that says how long to wait (how GitHub's API answers a rate limit), timeouts and requests that got no answer, waiting as long as a Retry-After or the GitHub API's rate-limit reset asks, or a minute for its secondary limit, measured on the server's clock (a wait over two minutes is not retried). Each retry is a plain log line, not a warning annotation. Anything else outside 2xx throws an `HttpError`. Only idempotent methods are retried by default: a POST or PATCH gets one attempt unless the caller sets `attempts`, knowing a repeat is harmless. The timeout covers reading the body too, so size it for the download. A stream body needs `attempts: 1`, since a retry would send it again. A caller's own `signal` aborts the request and ends the retries. A URL with a user or password is refused, as is one that does not parse, and neither error shows the URL. |
| `HttpError` | Its message holds the method, the URL without its query string (where a pre-signed URL carries its signature) and the status. `body` holds the start of the response's body, where an API says why it refused. That stays out of the message and the logged stack, since a server may echo credentials there; add it to an error of your own when you trust the server. |
| `retry` | The same bounded retry for any other step that can fail transiently, such as a download and its checksum as one unit. Give a `fetchWithRetry` inside it `attempts: 1`, or the two retry counts multiply (3 × 3 requests). `attempts` must be a whole number of 1 or more. The default `isRetryable` is `isTransient`, which retries no bug in the callback. A Retry-After over two minutes is never waited out, whatever `isRetryable` says. An abort during a wait throws an AbortError whose cause is the error being retried. |
| `maskUrl` | Mask a URL that is the operator's own, such as the LAN cache's: the URL, its host and its hostname. Returns it without trailing slashes; one that does not parse throws without showing it. |
| `requestFailure` | Why a request failed, without the address it went to: the HTTP status, or the error codes along its causes. `fetch`'s own messages name the address it connected to, which no mask of a host name covers. |
| `command` | Runs a program with its arguments, never through a shell, and resolves to what it prints. An exit other than 0 rejects with the last line of its stderr, and a program that cannot start with its spawn error (`ENOENT`). |
| `execute` | Runs a program as the `run:` step it replaces would have: never through a shell, with no stdin, in the directory `cwd` names or the action's own, with the environment without the runner's variables unless given another, and its output copied to the log as it comes (stdout left out with `echo: false`). Resolves to what it printed on stdout and rejects as `command` does; when its `signal` aborts, the program is sent SIGTERM and it rejects with an AbortError. |
| `withoutRunnerVariables` | An environment without the variables the runner gives a JavaScript action and not a `run:` step, `ACTIONS_*` and `INPUT_*`, for a program the action starts (below). |
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
- **A tool CI runs at its latest release**, by decision, is the
  exception: `install-conformu`, since hardware-validation.md's version
  gate wants every new record on the latest ConformU. Its `resolve` asks
  GitHub's API for the latest release and pins each asset to the SHA-256
  the API gives for it (the asset's `digest`, which GitHub computes on
  upload, for assets uploaded since mid-2025); a release without one is
  refused. It caches nothing, so it is one JavaScript action rather than
  the composite above, and resolves and installs in one step, where the
  release cannot move between them.
- **An installer the vendor ships** runs as the vendor meant: ConformU's
  Windows installer (Inno Setup) runs silently from a scratch directory
  and installs where its script says, which no command line moves, and
  needs an administrator; a macOS disk image is mounted read-only,
  without the Finder, inside the work directory (`hdiutil attach
  -nobrowse -mountpoint`), its app copied out with `ditto`, and detached
  however the copy ends.
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
  `install` exports them to the job. A `resolve` may answer with a
  promise, as ConformU's does, which reads the latest release from
  GitHub's API, unauthenticated on a developer's machine; ConformU's
  action has no `install`, and exports what `installAt` resolves to. `local-install/local.mjs` lists the
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
  looks. ConformU's on Windows is where its installer installs it,
  `C:\Program Files\ASCOM\ConformU`, and any other is refused there.
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

`bdd-sharded` starts the BDD suites as processes at once and waits for
them all, and `conformu-run` the ConformU suites. What they share is
`lib/processes.mjs`: `runStreams` starts streams of programs at once,
each stream's programs one after another, and `replay` prints each
stream's log as a group.

- **`spawn` a program, never through a shell**, with stdin `'ignore'`:
  a program that read stdin would take the step's, and in a list run one
  after another, the next one's. `command` in the helper module does this
  for one program whose output the action reads, and `execute` for one
  whose output belongs in the step's log, with the environment below.
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
  children and prints what it must within the 7.5 s. `runStreams` sends
  each program SIGTERM, then SIGKILL 5 s later, and the action prints the
  logs.
- **A stop goes to the program's process group.** On Linux and macOS
  `runStreams` starts each program as the leader of a process group of its
  own (`detached`) and signals the group, since what a program starts
  outlives a signal sent to it alone: SIGTERM ends `cargo test` and leaves
  its test binary running. A process that starts a session of its own
  leaves the group, as bdd-infra starts ConformU; it runs on until its own
  deadline, or until the runner kills it at the job's end with every
  other process whose environment holds the job's `RUNNER_TRACKING_ID`
  (actions/runner, `src/Runner.Worker/JobExtension.cs`), which is why no
  action removes that variable. Windows has no group a signal reaches, and
  there the program is killed.
- **A program the action downloaded and nobody has checked** gets the
  step's environment without the runner's `ACTIONS_*` variables and the
  action's `INPUT_*` (`withoutRunnerVariables`, which `execute` applies
  unless given an environment). So does a program the action runs where
  a `run:` step ran it, so that the move hands it nothing the step did
  not: `bazel-coverage` and `rules-rust-patches` run Bazel so, which
  runs the build's repository rules, `uncovered-diff-lines` runs pip,
  diff-cover and the coverage scripts' poster so, `cargo-workspace` runs
  Cargo and cargo-msrv so, `conformu-run` runs each service's `cargo
  test` so, `beta-clippy` runs clippy and the census script so,
  `install-astap`'s smoke test runs ASTAP so, `install-conformu`
  runs ConformU's Windows installer and each install's `--version` so,
  and `packages/repo-site` runs dpkg-scanpackages, apt-ftparchive,
  createrepo_c, gpg and podman so. A
  JavaScript action gets `ACTIONS_RUNTIME_TOKEN`, which can write the
  repository's caches, and a `run:` step does not (actions/runner,
  `NodeScriptActionHandler.cs`). Its job also runs with `contents: read`
  and `persist-credentials: false`, so no token is on disk.
  `refresh-astap`'s `detect` runs a new ASTAP release so. That only
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

### Code another program runs

`remote-cache` is the example: it writes the Bazel steps' `ci-remote`
config, and Bazel runs its credential helper in each Bazel step, for the
write credential.

- **A launcher names the runner's own Node** (`process.execPath`): the
  other program starts the code as a program, and a pool runner's PATH may
  have no Node. It is a shell script on Linux and macOS and a `.cmd` file
  on Windows, written to `RUNNER_TEMP`.
- **Its stdout is the other program's to parse**, so the code prints no
  workflow command and does not use `run`: a failure goes to stderr and
  the exit status.
- **A secret stays in the environment.** The code reads it from the step
  the other program runs in, where the job's `env` block puts it, never
  from a file the action wrote. For Bazel that rules out an rc file above
  all: `--announce_rc`, which `build:ci` sets, prints every option Bazel
  reads from one.
- **Its tests run the launcher as the program does**: directly on Linux
  and macOS, and through cmd.exe on Windows, where CreateProcess runs a
  `.cmd` file so.

### An action that calls the GitHub API

`lib/github.mjs` holds what `tracking-issue`, `refresh-astap`,
`uncovered-diff-lines`, `install-conformu`, `beta-clippy`,
`commit-repin` and `packages` share: `runContext` (the API, the
repository and the run's link from the runner's variables), `github` (a
request that names what failed and carries GitHub's own answer, and a
rate limit's wait as its `retryAfterMs` for `retry`; a POST or PATCH
sent once; with `json` or `upload`, a body, such as a release asset's
bytes; with `missing`, a 404 is no error; with `accept`, the answer in
another media type, such as a pull request's diff), `everyPage` (every
item of a paged list, a next page on another origin refused),
`openIssues` (every page of the open issues with a label, pull requests
left out) and `commentOrOpen` (the open issue with a label and an exact
title gets a comment, or one is opened; an issue opened without the
label fails the step, since GitHub drops a label the repository lacks
and says nothing).

- **The token is an input** (`default: ${{ github.token }}`), and the
  job grants only what the action needs, beside `contents: read`. Without
  one, `github` sends no Authorization header, for a developer's install
  that reads a public repository (`install-conformu`).
- **A change to the repository goes through the API**, not a push from a
  checkout: `refresh-astap` reads the file at main's tip (`git/ref`,
  `git/commits`, `contents`), writes a tree on main's and a commit on
  main (`git/trees`, `git/commits`), and creates or moves its branch to
  it (`git/refs`). Its job checks out the action only, with no token
  left on disk, and holds no clone to push from. `commit-repin` commits
  the `MODULE.bazel.lock` repin-bazel.yml refreshed on a dependabot
  branch so: a blob, a tree on the checked-out commit's, a commit on it,
  and the branch moved to it as a fast-forward, so a branch that moved
  meanwhile fails the step rather than lose a commit. Its job's checkout
  keeps no token either, and the token reaches that step alone, as its
  input. `packages/tap` commits a channel's Homebrew formulas to the
  tap, another repository, the same way, with the tap's token as its
  input, and `packages/publish-nightly` moves the nightly tag and
  rewrites its release, deleting and uploading its assets.
- **Tests run against a fake API** (`serve`) that answers in the shapes
  the real one gives; record a real answer to each read with
  `gh api` and take the writes' from GitHub's REST docs.
- **Another service's API** has its own client beside the action:
  `packages/r2.mjs` puts, reads and deletes objects through R2's S3 API,
  signing each request with Signature Version 4 on `node:crypto`, as the
  bucket-scoped token R2 gives the nightly can only use that API. Its tests
  check the signature against the SigV4 test suite's and against the one
  the aws CLI gave an upload, and publish against a fake bucket.

### Proving it

A PR's CI only runs the workflows its events trigger. Name every runner
the code runs on (hosted Linux, Windows and macOS, the pool's Linux and
Windows templates, the Pi) and show each one green, or say what stays
unproven until it first runs on main and watch that run. The plan's
[Working an item](../plans/archive/ci-shell-to-code.md#working-an-item) covers the
workflows that cannot be dispatched on a branch.

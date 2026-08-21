# `rusty-photon-service-lifecycle` Crate Design

Unified service lifecycle for every long-running Rusty Photon binary:
owns the tokio runtime, installs OS signal handlers (or dispatches to
the Windows Service Control Manager), and exposes a single cooperative
shutdown handle backed by [`tokio_util::sync::CancellationToken`].

This is a workspace library, not a service. Every service binary in
`services/` consumes it directly. For the migration plan that moves
existing services onto it, see
[`docs/plans/archive/service-lifecycle-unification.md`](../plans/archive/service-lifecycle-unification.md);
this doc covers the crate's own design.

## Scope

- Build the tokio runtime; invoke a user closure on it with a
  [`Shutdown`] handle.
- Install OS signal handlers (Ctrl+C + SIGTERM on Unix; Ctrl+C +
  Ctrl+Break on Windows console) without panicking on install failure.
- Optional: install a SIGHUP-driven reload notifier
  ([`ReloadSignal`]).
- Optional (cargo feature `scm`, Windows-only): dispatch to the
  Windows Service Control Manager and translate
  `ServiceControl::Stop` / `ParamChange` into the same `Shutdown` +
  `ReloadSignal` types the console path produces.
- Cooperative-shutdown propagation: the handle is a
  `CancellationToken`-backed clone, so spawned subtasks observe the
  same cancellation as the main loop.
- Install the shared tracing subscriber ([`init_tracing`] /
  [`init_service_tracing`]): logs go to stderr (stdout is reserved for the
  `bound_addr=` handshake the BDD harness parses), filtered by `RUST_LOG`
  when set, otherwise at a caller-supplied fallback level. Idempotent.
  Every service binary calls this once at startup so logging is configured
  identically everywhere. The subscriber carries a
  `tracing_error::ErrorLayer` so panic reports include the active span
  stack.
- SCM-mode rolling-file logging ([`init_service_tracing`], cargo feature
  `scm`): when a service runs under the Windows Service Control Manager
  (`--service`), both std handles are dead, so the stderr writer is swapped
  for a `tracing-appender` rolling file
  `%PROGRAMDATA%\rusty-photon\logs\<svc>.<date>.log` (daily rotation,
  14 files retained). Console mode is byte-for-byte unchanged. See
  [ADR-015](../decisions/015-windows-packaging-architecture.md).
- Own top-level error & panic reporting ([ADR-011](../decisions/011-error-reporting-layers.md)):
  the runner installs the `color-eyre` hooks once per process, so panics
  print formatted reports with span context on every service; run-closure
  errors are converted into a `color_eyre::Report` at the runner's return,
  so `main` prints a readable multi-line `source()` chain instead of a
  single-line `Debug` dump. This crate is the **only** owner of
  `color-eyre`/`tracing-error` in the workspace — errors stay
  `thiserror`-typed everywhere below the binary boundary, and only the
  *types* (`ServiceResult`, `RunError`, `RunResult`, `Report`) plus the
  chain-preserving [`report_from_boxed`] conversion are re-exported, never
  the `eyre!`/`bail!` macros.
- Carry the workspace's **little-endian target guard**. This crate is not
  about endianness, but it is the one crate every binary depends on, and
  Cargo.toml cannot express a target restriction — so the
  `#[cfg(not(target_endian = "little"))] compile_error!` in its crate root is
  what makes a big-endian build fail everywhere rather than silently
  byte-reverse a star catalog. See
  [workspace.md § Supported targets](../workspace.md#supported-targets).

Out of scope:

- Owning CLI argument parsing. Services keep `clap`; the runner takes
  a service name and a closure. (Services still own their `--log-level`
  flag and pass its value to [`init_tracing`] as the fallback level.)
- Defining what graceful shutdown means for any particular server.
  Services compose `shutdown.cancelled()` into their own server stop
  (a `tokio::select!` race, `axum::with_graceful_shutdown(...)`, or
  a passed-in `CancellationToken`).
- Replacing `#[tokio::main]` as a macro. The runner is a plain
  builder; the user closure is plain `async`.
- Windows Service installation artifacts (msi, winget, `sc create`
  scripts). The crate produces a *binary capable of running under
  SCM*; packaging is a separate concern.

## Public API

Four types, three result/error aliases, and four free functions
([`init_tracing`], [`init_service_tracing`], [`is_scm_service`],
[`report_from_boxed`]), plus the type-only `color_eyre::Report`
re-export.
[`Shutdown`] is constructed only by the runner.
[`ReloadSignal`] has a public constructor so integration tests can
drive a service's run loop with synthetic reload events, and so
non-signal-driven reload sources (e.g. a file-watcher) can share the
same primitive — but the canonical producer remains the runner.

```text
ServiceRunner::new(name)                       -> ServiceRunner
ServiceRunner::with_reload(self)               -> ServiceRunner
ServiceRunner::scm_mode(self, enable)          -> ServiceRunner   // no-op unless cfg(all(windows, feature = "scm"))
ServiceRunner::run(self, |Shutdown| async)              -> ServiceResult
ServiceRunner::run_with_reload(self, |Shutdown, ReloadSignal| async) -> ServiceResult

Shutdown::token()        -> CancellationToken
Shutdown::cancelled()    -> impl Future<Output = ()> + Send + 'static
Shutdown::is_cancelled() -> bool

ReloadSignal::new()      -> ReloadSignal      // for tests / alt sources
ReloadSignal::notify(&self)                   // wake one waiter
ReloadSignal::recv(&self) -> impl Future<Output = ()> + '_

init_tracing(default_level: tracing::Level)   // stderr + RUST_LOG/EnvFilter subscriber + ErrorLayer; idempotent

init_service_tracing(service_name: &str, default_level: tracing::Level, scm_mode: bool)
    -> TracingGuard
    // what service binaries call: init_tracing in console mode; in SCM mode
    // (Windows + `scm` feature + scm_mode=true) a rolling-file subscriber.
    // `main` must hold the guard (`let _tracing_guard = ...`) until process
    // exit so the non-blocking writer flushes its final lines on SCM Stop.

is_scm_service() -> bool
    // true once SCM mode has engaged (sticky, process-global; always false
    // in console mode / off Windows). Gates the raw std-handle writes that
    // remain on the service path — notably the stdout `bound_addr=`
    // handshake: `if !is_scm_service() { println!("... bound_addr={addr}") }`.
    // The BDD harness never passes --service, so port discovery is unaffected.

type ServiceResult = Result<(), color_eyre::Report>   // what main returns
type RunError      = Box<dyn Error + Send + Sync>     // what run closures return (error side)
type RunResult     = Result<(), RunError>             // what run closures return
report_from_boxed(RunError) -> Report          // chain-preserving conversion for pre-runner `?` sites
pub use color_eyre::Report                     // type-only re-export; eyre!/bail! are NOT re-exported
```

The closure passed to `run` / `run_with_reload` is `FnOnce(...) -> Fut`
where `Fut: Future<Output = RunResult> + 'static`
and the closure itself is `Send + 'static`. The runner converts the
closure's boxed error into the `Report` that `main` returns (preserving
the `source()` chain) — closures keep using `?` on typed and boxed errors
alike, and never construct `Report`s themselves (ADR-011). Bounds explained:

* `F: Send + 'static` — the SCM dispatch path stashes the closure in
  a `OnceLock` and re-enters it from the `extern "system" fn` service
  entry point, which requires both bounds. The console path inherits
  them for API uniformity.
* `Fut: 'static` — the SCM path type-erases the future into
  `Pin<Box<dyn Future<Output = RunResult>>>`, which is implicitly
  `+ 'static`. Most async fn bodies satisfy this naturally because
  they own their captures via `move` semantics; the only futures that
  fail are those that borrow non-`'static` data.
* `Fut: Send` is **not** required. `Runtime::block_on` polls the
  future on the calling thread, so error types and intermediate state
  inside the closure body stay non-`Send`-friendly without extra
  bounds.

## Behavior

### Signal install (no-panic)

The runner spawns a single signal-watcher task that races the OS
signals and cancels the underlying [`CancellationToken`] on first
fire. Install failures are logged via `tracing::warn!` and replaced
with a never-resolving future, so a misconfigured environment that
cannot install (e.g., already-stolen signal) degrades to "the other
signal source still works" rather than "the service panics during
startup":

```rust
// Registers on call, and hands back the future that resolves on first fire.
#[cfg(unix)]
fn install_shutdown_signals(name: &'static str) -> impl Future<Output = ()> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = install(name, "Ctrl+C", signal(SignalKind::interrupt()));
    let mut terminate = install(name, TERMINATE_EVENT, signal(SignalKind::terminate()));
    async move {
        tokio::select! {
            () = next_signal(&mut interrupt) => { /* Ctrl+C */ }
            () = next_signal(&mut terminate) => { /* SIGTERM */ }
        }
    }
}
// Windows is the same shape over `windows::ctrl_c()` and `windows::ctrl_break()`.
```

`install` logs a failed registration via `tracing::warn!` and yields
`None`, which `next_signal` awaits as a never-resolving future — so a
misconfigured environment that cannot install one signal (e.g. an
already-stolen handler) degrades to "the other source still works"
rather than "the service panics during startup". This matches the
no-panic pattern established workspace-wide by PR #289. One
implementation; one log-message style; one place to fix bugs.

`next_signal` is a generic function over a private `SignalHandler`
trait, not a macro, because the platform handler types share no upstream
trait (Windows has a distinct type per event). The choice also keeps the
degraded `None` arm coverable: a macro expands into its caller, so a
test exercising it would attribute the arm to the test module — which
coverage excludes, leaving the path both untested and invisible.

### Handlers install before the service starts

`ServiceRunner` awaits handler registration before invoking the user
closure. The watcher signals a `oneshot` the moment installation
returns, and the runner's `block_on` waits on it:

```rust
let installed = spawn_shutdown_watcher(&rt, name, token.clone());
rt.block_on(async move {
    let _ = installed.await;
    run_fn(Shutdown::from_token(token)).await
})
```

This ordering is a correctness requirement, not a nicety. Until the
handlers exist the platform default disposition applies, and on both
platforms that default is fatal: SIGTERM terminates, and an
unregistered Windows console control event falls through to a handler
that terminates. A service that reached its own readiness handshake
first — the `bound_addr=` line, a health endpoint answering — would be
advertising itself as stoppable during a window in which being stopped
kills it outright. Services do enough async setup that the window is
narrow, but "narrow" is what the unhandled-Ctrl+Break bug was too.

It is also why every constructor above registers **synchronously**.
`tokio::signal::ctrl_c()` registers only when its future is first
polled, so it cannot be sequenced ahead of anything; the platform
constructors (`unix::signal`, `windows::ctrl_c`, `windows::ctrl_break`)
return `io::Result<_>` on call and can. Splitting install-from-await is
what makes the barrier expressible.

`handlers_install_before_the_closure_runs` guards it by raising SIGTERM
as the closure's first statement, with no delay — survivable only if
the handler is already in place. A sleep there would let installation
catch up and make the test vacuous.

**Both Windows console events must be registered.** `ctrl_c()` covers
CTRL_C_EVENT only; CTRL_BREAK_EVENT is a separate registration, and it
is the event a supervisor sends to stop a console-mode service by
process group (`GenerateConsoleCtrlEvent` — what `bdd-infra`'s
`ServiceHandle::stop` uses). An *unregistered* console control event
falls through to the OS default handler, which terminates the process
outright: no cancellation, no shutdown path, no flush, and no error
raised anywhere. The failure is silent by construction — the only
evidence is the absence of the shutdown log line — so a test that
asserts a service stopped *quickly* cannot detect it. `bdd-infra`'s
`test_stop_runs_the_service_shutdown_path` guards this by requiring an
effect the shutdown path itself produces.

CTRL_CLOSE / CTRL_SHUTDOWN / CTRL_LOGOFF stay unregistered. They are
session-teardown events with an OS-imposed handler deadline, and the
deployed Windows path is a service, which receives `ServiceControl::Stop`
instead (below) and never sees them.

### Reload (opt-in via `with_reload`)

When [`ServiceRunner::with_reload`] is set, the runner additionally
spawns a SIGHUP-watcher (Unix) — or, in SCM mode, wires the
`ServiceControl::ParamChange` event — to a [`ReloadSignal`] that the
user closure receives via `run_with_reload`. Each event wakes one
waiter via `Notify::notify_one`. The signal is *notification only*:
re-reading config and rebuilding state is the caller's job.

On Windows console mode there is no SIGHUP equivalent;
`ReloadSignal::recv` returns a never-resolving future.

### Shutdown propagation

The runner constructs one [`CancellationToken`] per `run` invocation.
The signal-watcher task cancels it on first fire; SCM mode cancels it
when the control handler receives `ServiceControl::Stop`. The user
closure receives a [`Shutdown`] wrapping this token; `.token()` hands
out clones for spawned subtasks. All clones observe the same
cancellation — propagation is `O(1)`, not `O(N)` per-subtask wakes.

This is the existing `sentinel` pattern, generalized: sentinel
already takes a `CancellationToken` through its engine and cancels it
from a hand-rolled signal task. After the migration, that task goes
away and sentinel's engine receives `shutdown.token()` directly.

**Do not install a second signal handler downstream.** If a server
inside your service already takes a shutdown future (e.g. axum's
`with_graceful_shutdown`, or `BoundServer::start` after the #294
migration), pass `shutdown.cancelled()` *into* it rather than racing
it externally with `tokio::select!`. Racing two independent signal
sources lets one drop the other mid-flight — that's the bug
[#287](https://github.com/rusty-photon/rusty-photon/issues/287) and
the underlying motivation for funneling everything through a single
`Shutdown` handle.

### SCM mode (`#[cfg(all(windows, feature = "scm"))]`)

When `scm_mode(true)` is set on Windows with the `scm` feature
enabled, `run` / `run_with_reload` take a different path:

1. Stash the runner config + closure in a `OnceLock` (the SCM
   dispatch entry point is a no-arg `extern "system" fn`, so the
   closure must be reachable from a static).
2. Call `windows_service::service_dispatcher::start(name, ffi_main)`.
3. Inside `ffi_main`, register a control handler translating:
   - `ServiceControl::Stop` → `token.cancel()`
   - `ServiceControl::ParamChange` (only when `with_reload`) →
     `reload.notify()`
   - `ServiceControl::Interrogate` → `NoError`
   - everything else → `NotImplemented`
4. Report `ServiceState::Running` to SCM.
5. Build the tokio runtime, invoke the user closure with the same
   [`Shutdown`] (and optional [`ReloadSignal`]) types as the console
   path. **The user closure does not know whether it is running under
   SCM or under console-mode signals** — that is the whole point of
   the abstraction.
6. On the closure's return, report `ServiceState::Stopped`. The
   `exit_code` field reflects the closure's outcome: `Win32(0)` on
   `Ok(())`, `ServiceSpecific(1)` on `Err` (i.e. `dwWin32ExitCode =
   ERROR_SERVICE_SPECIFIC_ERROR`, `dwServiceSpecificExitCode = 1`).
   Reporting `0` on every stop would let crashes look like clean
   shutdowns in `services.msc` and any supervisor reading SCM's stop
   record. A closure error is additionally logged (under SCM the
   rolling log file is often the only visible trace) and stashed so
   `dispatch` can return it from `main` once
   `service_dispatcher::start` unblocks — SCM mode keeps console
   mode's "`main` returns the closure's error" contract.

**Failure visibility / restart contract (ADR-015, windows-packaging
W1).** The non-zero exit code on `Err` is the pinned mechanism for
making a deliberate service exit (e.g. a serial driver's
eager-validation failure) count as a *failure* to SCM: the installer
configures SCM failure actions (restart after 5 s) **and** sets
`SERVICE_CONFIG_FAILURE_ACTIONS_FLAG` (failure actions on non-crash
failures), so a `SERVICE_STOPPED` report with a non-zero exit code
triggers the configured restart — reproducing the systemd
`Restart=on-failure`/`RestartSec=5` contract. The alternative
(exiting the process without reporting `SERVICE_STOPPED`) was
rejected: it works without the failure-actions flag, but the SCM
record then carries no exit code and every deliberate exit shows up
as a crash (event 7034) instead of a clean stop record with a cause
(event 7024). Clean stops (`Ok(())` — e.g. after an SCM Stop
request) report `Win32(0)` and trigger no restart.

When `scm_mode(false)` (or the `scm` feature is off, or the target
is not Windows), the runner falls through to the OS-signal path
unchanged.

### SCM-mode rolling-file logging (`init_service_tracing`)

Under SCM both std handles are absent, so a service-mode process
logging to stderr logs into the void. Service binaries therefore call
[`init_service_tracing`] (not plain [`init_tracing`]) at startup,
passing their `--service` flag value:

- **Console mode** (`scm_mode = false`, or any non-Windows target, or
  the `scm` feature off): behaves exactly like [`init_tracing`] —
  stderr, `RUST_LOG`/fallback filtering, `ErrorLayer`. Byte-for-byte
  unchanged output.
- **SCM mode** (`cfg(all(windows, feature = "scm"))` and
  `scm_mode = true`): the fmt layer writes to a `tracing-appender`
  rolling file `%PROGRAMDATA%\rusty-photon\logs\<svc>.<date>.log` —
  daily rotation, 14 files retained, ANSI disabled. The ProgramData
  root resolves from the `ProgramData` environment variable with a
  `C:\ProgramData` fallback (the resolver is private to this crate;
  `rusty-photon-config` grows its own for the W2 config path —
  dedup is a noted follow-up). The writer is non-blocking: the
  returned [`TracingGuard`] owns the worker guard and **must be held
  in `main` until process exit** (`let _tracing_guard = ...`) so the
  final shutdown-path lines flush on SCM Stop. If the log file cannot
  be opened (unusable ProgramData ACL), init falls back to the stderr
  subscriber instead of failing service startup. Idempotent like
  [`init_tracing`]: a redundant call (a global subscriber is already
  installed) keeps the existing subscriber and returns an *inert*
  guard — the freshly built writer's worker is dropped rather than
  leaked, since no events would ever route to it.

The filter/`ErrorLayer` composition is identical in both modes; only
the writer differs. A run-closure error that ends an SCM service is
rendered (full `source()` chain) into the rolling file via
`tracing::error!` before the runner returns it from `main` — the
`Report` that `main` prints goes to stderr, which is dead under SCM.

**Raw std-handle writes are gated on [`is_scm_service`].** SCM mode is
recorded in a sticky process-global flag (set by `init_service_tracing`
and, belt-and-braces, by the SCM dispatch itself). Anything a service
still writes to a raw std handle on its service path checks it:

- every service's stdout `bound_addr=` handshake is wrapped in
  `if !is_scm_service() { println!(...) }` — the only stdout consumer
  is `bdd-infra`'s port parser, which never runs services with
  `--service`, so console/BDD port discovery is unaffected;
- pre-runner error prints go through `tracing::error!` instead of
  `eprintln!` (plate-solver's startup-config failure is the precedent):
  in console mode the subscriber writes to stderr anyway, in SCM mode
  the message lands in the rolling file instead of a dead handle.

In the common case a dead-handle write would merely sink (Rust's
Windows stdio treats absent handles as successful sinks) — but sunk
means *lost diagnostics*, and gating is also belt-and-braces against a
genuinely invalid (non-NULL, closed) handle. Confirming benign
std-handle behavior under a real SCM stays a `verify-msi.ps1` smoke
item (windows-packaging plan, W4/W5).

### Why the runner owns the runtime

`ServiceRunner::run` is a synchronous `fn`, not an `async fn`. It
builds a `tokio::runtime::Runtime` internally and `block_on`s the
user closure on it. This choice is forced by the SCM dispatch path:
`service_dispatcher::start` is synchronous and must be called from a
context that owns the entry point — there is no tokio runtime when
`ffi_main` is invoked, so SCM and console paths cannot share a single
entry point unless the runner owns runtime construction. The same
shape works for console mode without any compromise.

Practically, this means services move from `#[tokio::main] async fn
main() -> Result<...>` to `fn main() -> Result<...> { runner.run(|s|
async move { ... }) }`. The visible diff is small, and there's only
one place in main where `async` enters.

## Usage

### ASCOM Alpaca driver, console only

The common case (10 of 12 services after migration). The server
consumes `shutdown.cancelled()` directly — there is no outer
`tokio::select!`. This avoids the double-installation race described
in [issue #287](https://github.com/rusty-photon/rusty-photon/issues/287)
by making axum's `with_graceful_shutdown` (inside `BoundServer::start`)
fire on the same source as the OS-signal watcher.

```rust
use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner};

fn main() -> ServiceResult {
    let args = Args::parse();
    init_tracing(args.log_level);

    ServiceRunner::new("dsd-fp2").run(|shutdown| async move {
        let bound = ServerBuilder::new()
            .with_config(args.config)
            .build()
            .await?;

        // `BoundServer::start` accepts the shutdown future and threads
        // it into axum's `with_graceful_shutdown`. When the signal fires,
        // axum drains in-flight requests before returning.
        bound.start(shutdown.cancelled()).await?;
        Ok(())
    })
}
```

For services where the server still does its own
`tokio::select!` internally (e.g. the engine in `sentinel`),
pass `shutdown.token()` instead — see the next example.

### Axum service (graceful shutdown drains in-flight requests)

```rust
ServiceRunner::new("calibrator-flats").run(|shutdown| async move {
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.cancelled())
        .await?;
    Ok(())
})
```

`with_graceful_shutdown` is axum's idiomatic stop signal; the runner's
`Shutdown` plugs in directly with no `tokio::select!` glue.

### Service with spawned worker tasks (sentinel-style)

When you spawn workers that need to know about shutdown, hand them
clones of the token:

```rust
ServiceRunner::new("sentinel").run(|shutdown| async move {
    let token = shutdown.token();

    let dashboard = tokio::spawn(run_dashboard(token.clone()));
    let engine = tokio::spawn(run_engine(token.clone()));

    let _ = tokio::join!(dashboard, engine);
    Ok(())
})
```

The token is cloned at no cost; cancellation propagates to every
clone, so each worker can `tokio::select! { _ = token.cancelled() => ... }`
in its own loop.

### Service with reload (filemonitor)

Enable reload, optionally enable SCM mode (gated on a CLI flag the
SCM control manager passes via `binPath`), and use
`run_with_reload`:

```rust
fn main() -> ServiceResult {
    let args = Args::parse();
    // SCM mode: rolling file under %PROGRAMDATA%\rusty-photon\logs\.
    // Console mode: stderr, unchanged. Hold the guard until process exit.
    let _tracing_guard = init_service_tracing("filemonitor", args.log_level, args.service);

    ServiceRunner::new("filemonitor")
        .with_reload()
        .scm_mode(args.service)       // requires `features = ["scm"]`
        .run_with_reload(|shutdown, reload| async move {
            run_server_loop(&args.config, shutdown.token(), reload).await
        })
}
```

`run_server_loop` then races its config-rebuild loop against
`shutdown.token().cancelled()` and `reload.recv()` in a single
`tokio::select!`. The same closure works under SCM (driven by
`Stop` / `ParamChange`) and console (driven by `SIGTERM` /
`SIGHUP`); the service body sees identical types.

### Enabling SCM in a new service

Every service binary in `services/` already has this (windows-packaging
plan, W1). For a new one, three changes:

```toml
# services/<name>/Cargo.toml
# Enable the Windows Service Control Manager dispatch only on Windows;
# on Unix the `scm` feature would pull in `windows-service` for no
# runtime benefit.
[target.'cfg(windows)'.dependencies]
rusty-photon-service-lifecycle = { workspace = true, features = ["scm"] }
```

```rust
// services/<name>/src/main.rs
#[derive(Parser)]
struct Args {
    // ... existing args ...

    /// Run as a Windows service (used by the service control manager).
    /// No-op on non-Windows targets.
    #[arg(long, hide = true)]
    service: bool,
}

fn main() -> ServiceResult {
    let args = Args::parse();
    let _tracing_guard =
        init_service_tracing("my-service", args.log_level, args.service);

    ServiceRunner::new("my-service")
        .scm_mode(args.service)   // no-op on non-Windows targets
        .run(|shutdown| async move {
            // ...
        })
}
```

(The flag is deliberately *not* `#[cfg(windows)]`-gated: it parses — and
is a documented no-op — everywhere, so the CLI surface is identical
across platforms.) No per-service Bazel wiring is needed: the service's
`BUILD.bazel` depends on the plain
`//crates/rusty-photon-service-lifecycle` label, and the lifecycle
library target itself enables the `scm` feature via a `crate_features`
`select()` on Windows. That keeps a single crate instantiation per
platform across every consumer, direct or transitive — a per-consumer
`_scm` library variant was tried and reverted because two
instantiations in one binary's graph stop the crate's types
(`ReloadSignal`, `Shutdown`) from unifying (E0308 on Windows).

That's the whole adoption cost. The SCM control-handler glue and the
rolling-file logging live in the crate; the service binary just opts in.

## Module Layout

```
crates/rusty-photon-service-lifecycle/src/
├── lib.rs       # crate root: //! docs, module decls, re-exports
├── logging.rs   # init_tracing / init_service_tracing + TracingGuard;
│                # `mod scm_file` (feature `scm`) holds the rolling-file
│                # writer + ProgramData/log-dir resolution — compiled on
│                # every OS so its helpers are unit-tested cross-platform
├── shutdown.rs  # Shutdown — thin wrapper over CancellationToken
├── reload.rs    # ReloadSignal — thin wrapper over Arc<Notify>
└── runner.rs    # ServiceRunner — builder + run/run_with_reload
```

There is intentionally no `scm.rs` module today; the SCM dispatch
code lives inline in `runner.rs` behind `#[cfg(feature = "scm")]`.
If the SCM glue grows past ~100 LOC it gets its own module.

## Testing

- **Unit tests** live next to the code under
  `#[cfg(test)] mod tests`. The cross-cutting primitives ([`Shutdown`]
  and [`ReloadSignal`]) are trivial enough that their tests focus on
  contract — `cancelled()` resolves when any clone cancels, `recv()`
  resolves on `notify()`, `is_cancelled()` flips after `cancel()`.
- **Signal-install integration tests** drive the full
  `ServiceRunner::run` / `run_with_reload` path on Unix: spawn a
  task that `libc::raise`s `SIGTERM` (or `SIGHUP` followed by
  `SIGTERM`) after a brief delay, then assert that
  `shutdown.cancelled()` resolves (and that `reload.recv()` fires
  at least once for the SIGHUP variant). Tokio's signal handlers
  are reference-counted, but raising signals to self affects the
  whole process, so the test module serializes runs via a
  `std::sync::Mutex<()>`. `libc` is a `cfg(unix)` dev-dependency
  to avoid pulling in `nix` solely for these tests.
- **Runner contract** — `ServiceRunner::run` invokes the closure
  exactly once, propagates its `Result`, and returns `Ok(())` when
  the closure returns `Ok(())`. `run_with_reload` without a prior
  `.with_reload()` returns a descriptive error rather than running
  the closure. The runner builds and tears down a fresh runtime
  per call.
- **Error reporting** — the `Once`-guarded `color_eyre::install()` is
  idempotent across repeated `ServiceRunner::run` calls; a closure
  error with a `source()` chain renders as a multi-line report (chain
  present) rather than a single `Debug` line; the subscriber built by
  `init_tracing` carries the `ErrorLayer` (asserted via
  `SpanTrace::capture()` under `tracing::subscriber::with_default`).
- **SCM mode** — `scm_mode(false)` is a no-op and falls through to
  the OS-signal path on Windows. The full SCM dispatch is *not*
  exercised in CI: `windows_service::service_dispatcher::start` only
  succeeds when invoked by the actual SCM (it returns
  `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` otherwise). SCM mode is
  manually validated via filemonitor's existing Windows install/run
  procedure on each release; the windows-packaging plan's
  `verify-msi.ps1` (W4/W5) adds an installed-service smoke including
  a kill-and-observe restart check.
- **SCM logging** — the rolling-file helpers in `mod scm_file` are
  gated on the `scm` feature alone (not `cfg(windows)`), so their
  unit tests run cross-platform: ProgramData resolution is
  parameterized over the env value (set / unset / empty), the rolling
  writer is exercised against a temp dir (file named
  `<svc>.<date>.log`, content flushed on guard drop), and the
  fallback path is provoked by pointing the log dir under a regular
  file. Under Bazel these run via the
  `rusty-photon-service-lifecycle_scm_unit_test` target (all three CI
  OSes); under Cargo via
  `cargo test -p rusty-photon-service-lifecycle --features scm`.
- **No `coverage(off)` exclusions.** Per workspace policy, production
  code is covered or it isn't shipped. The dead-code arms in the
  no-panic signal-install path (`std::future::pending` fallbacks)
  are deliberately impossible to provoke in a unit test — they fire
  only when the OS refuses to install a handler — and we accept the
  coverage gap rather than write a `#[cfg(test)]` injection seam
  for a single branch.

## Dependencies

| Crate | Purpose |
|---|---|
| `tokio` | runtime, signal handling, `sync::Notify` |
| `tokio-util` | `CancellationToken` |
| `tracing` | warn/debug logging on signal events |
| `tracing-subscriber` | `init_tracing` subscriber (fmt + `EnvFilter`) |
| `color-eyre` | panic hook + `Report` formatting at the binary boundary (ADR-011) |
| `tracing-error` | `ErrorLayer` so panic reports carry the active `SpanTrace` |
| `tracing-appender` (opt, feature `scm`) | SCM-mode rolling log file + non-blocking writer |
| `windows-service` (opt, feature `scm`) | Windows Service Control Manager dispatch + control handler |

Workspace-pinned versions live in the workspace `Cargo.toml` — except
`color-eyre`, `tracing-error`, and `tracing-appender`, which are
**deliberately crate-local**: this crate is the sole owner of top-level
error/panic reporting (the structural guardrail that keeps `eyre!`-style
ad-hoc errors out of the `thiserror`-typed library/device code — see
[ADR-011](../decisions/011-error-reporting-layers.md) §3.4 of the plan)
and of the SCM rolling-file writer (no service touches `tracing-appender`
directly). The `scm` feature is opt-in per consumer — every service
binary enables it, but only under
`[target.'cfg(windows)'.dependencies]`, so Unix builds get zero
`windows-service`/`tracing-appender` content in their dep tree.

`windows-service` is `windows-service = "0.8"` — the Mullvad-
maintained Rust wrapper around the Windows Service Control API.
On non-Windows targets the crate's library is essentially empty, so
even if a consumer enables `scm` and is built on Linux/macOS the
runtime cost is zero.

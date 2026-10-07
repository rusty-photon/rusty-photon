# Skill: Service Lifecycle

## When to Read This

- When scaffolding a new long-running service binary
- When touching a service's `main.rs` or top-level shutdown handling
- When wiring a new entry point that needs SIGINT/SIGTERM, SIGHUP-on-reload,
  or Windows Service Control Manager dispatch
- When you see a hand-rolled `tokio::signal::ctrl_c` / `signal(SignalKind::terminate())`
  pair in a service and wonder whether it should be there

## Prerequisites

- Read the crate design: [`docs/crates/rusty-photon-service-lifecycle.md`](../crates/rusty-photon-service-lifecycle.md)
  — that's the authoritative reference for the public API, behavior,
  and SCM internals.

---

## Why this exists

Every long-running binary in the workspace needs the same three things:
a tokio runtime, OS signal handlers for graceful shutdown, and a way to
propagate "time to stop" to its workers. Before issue #294, twelve
services had drifted into four shapes for the same task — some panicked
on signal install failure, some spawned hand-rolled cancel tasks,
filemonitor carried ~100 lines of Windows SCM glue, and the five
shared-transport ASCOM services additionally raced two installations
against each other (issue #287).

`rusty-photon-service-lifecycle` is the single workspace-wide
replacement. One signal-install path, one propagation primitive
([`tokio_util::sync::CancellationToken`]), one handle (`Shutdown`),
optional SCM behind a cargo feature.

---

## The standard pattern

For any new service binary, this is the shape:

```rust
use rusty_photon_service_lifecycle::{init_service_tracing, ServiceResult, ServiceRunner};

fn main() -> ServiceResult {
    let args = Args::parse();
    // stderr in console mode; a rolling file in Windows SCM service mode.
    // Hold the guard until process exit (see the SCM section below).
    let _tracing_guard = init_service_tracing("my-service", args.log_level, args.service);

    ServiceRunner::new("my-service")
        .scm_mode(args.service)
        .run(move |shutdown| async move {
            let bound = ServerBuilder::new()
                .with_config(args.config)
                .build()
                .await?;
            bound.start(shutdown.cancelled()).await?;
            Ok(())
        })
}
```

Key points:

- `main` is **plain `fn`**, not `#[tokio::main]`. The runner owns the
  tokio runtime, so wrapping it with `#[tokio::main]` would nest two
  runtimes.
- Initialization that doesn't need async (clap parse, tracing init)
  stays outside the closure. Everything async — building the server,
  loading config that needs awaitable IO — lives inside.
- `shutdown.cancelled()` returns a `'static + Send` future. Hand it to
  any API that takes a shutdown future (`axum::serve(...).with_graceful_shutdown(...)`,
  the shared-transport `BoundServer::start(shutdown)`, …).
- `shutdown.token()` returns a `CancellationToken` clone for code that
  prefers tokens over futures (`tokio::select! { _ = token.cancelled() => ... }`,
  or APIs like sentinel's `with_cancellation_token`).
- Return type from the closure is `Result<(), Box<dyn std::error::Error + Send + Sync>>`
  (the crate's `RunResult`); `?` works on typed `thiserror` errors and boxed
  helpers alike. `main` returns the crate's `ServiceResult`: the runner
  converts the closure's error into a `color_eyre::Report`, so startup
  failures print the full `source()` chain, and it installs the `color-eyre`
  panic hook once per process (formatted panic reports with span context —
  see [ADR-011](../decisions/011-error-reporting-layers.md)). Never add
  `color-eyre`/`eyre` to a service or library crate; errors below the binary
  boundary stay `thiserror`-typed. For a rare fallible step *before*
  `ServiceRunner::run` whose helper returns a boxed error, convert with
  `rusty_photon_service_lifecycle::report_from_boxed`.
- Bootstrap the config file **before** the runner with
  `rusty_photon_config::resolve_and_init::<Config>(name, args.config, &default, identity_pointers)`:
  it resolves the path, materializes the serialized default config at the
  platform-default path on first start (same-host consumers — sentinel's
  health-probe derivation, doctor — read that file), and mints ASCOM
  `UniqueID`s into the given JSON pointers. A device section the file leaves
  out is filled in from `default` first. The file is written only when the
  result loads as `Config`, so a start that fails never edits the operator's
  file. `Config` implements `rusty_photon_config::ConfigFile`, with `check()`
  holding any rule the parse cannot express. Pass `&[]` when identities are
  hardware-derived or the service has no devices — declining minting must
  never silently drop the first-start materialization. Only services whose
  config is deliberately operator-provided (the `ConditionPathExists`-gated
  ones, `session-runner`) skip this and call `resolve_config_path` directly.
  Read the file with `rusty_photon_config::load_file::<Config>`: it tells a
  JSON syntax error apart from a file that is JSON but not a valid
  configuration. See
  [docs/crates/rusty-photon-config.md](../crates/rusty-photon-config.md).

---

## Plugging into a server

### Shared-transport ASCOM driver (`BoundServer::start(shutdown)`)

The five shared-transport services already take a shutdown future:

```rust
bound.start(shutdown.cancelled()).await?;
```

This is the only signal source — there is **no outer `tokio::select!`**
in `main` racing the server against another signal future. That double-
installation pattern is what issue #287 was about; the fix is to thread
a single source through.

### Plain axum service

`axum::serve` accepts a shutdown future directly. Inside your closure:

```rust
axum::serve(listener, app)
    .with_graceful_shutdown(shutdown.cancelled())
    .await?;
```

### Code that takes a `CancellationToken`

Some library code (e.g. `sentinel`'s engine) takes a `CancellationToken`
in its builder so workers can race against it. Use `shutdown.token()`:

```rust
SentinelBuilder::new(config)
    .with_cancellation_token(shutdown.token())
    .build()
    .await?
    .start()
    .await?;
```

The token is cloned cheaply and propagates to every clone — no
per-task wake-up cost.

### Long-running loops with multiple await points

When you have a `tokio::select!` already (e.g., a CLI's monitor mode
racing a receiver against a stop signal), bind the token once and race
on `token.cancelled()`:

```rust
let token = shutdown.token();
loop {
    tokio::select! {
        event = receiver.recv() => { ... }
        _ = token.cancelled() => break,
    }
}
```

`token.cancelled()` is cheap to re-await in a loop; each iteration
constructs a fresh future that observes the same cancellation.

---

## Reload (SIGHUP / SCM ParamChange)

Reload-capable services (filemonitor and the `config.apply` drivers)
enable it with `.with_reload()` and use `run_with_reload`:

```rust
ServiceRunner::new("my-service")
    .with_reload()
    .run_with_reload(|shutdown, reload| async move {
        loop {
            let config = load_config()?;
            tokio::select! {
                result = serve(config, shutdown.cancelled()) => return result,
                () = reload.recv() => continue,  // reload triggers config re-read
            }
        }
    })
```

`ReloadSignal::recv()` fires on Unix when SIGHUP is delivered, or when
SCM sends `ParamChange` in Windows service mode. On Windows console
mode there is no SIGHUP equivalent; `reload.recv()` returns a never-
resolving future.

---

## Windows Service Control Manager mode

SCM dispatch is opt-in per service via the `scm` cargo feature, and is
a no-op on non-Windows targets. **Every service binary in `services/`
has it** (windows-packaging plan, W1 — ADR-015). The uniform shape:

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
    // SCM mode logs to a rolling file (%PROGRAMDATA%\rusty-photon\logs\);
    // console mode logs to stderr unchanged. Hold the guard until process
    // exit so the final lines flush on SCM Stop.
    let _tracing_guard = rusty_photon_service_lifecycle::init_service_tracing(
        "my-service",
        args.log_level,
        args.service,
    );

    ServiceRunner::new("my-service")
        .scm_mode(args.service)   // no-op on non-Windows targets
        .run(|shutdown| async move {
            // ... same closure as console mode ...
        })
}
```

The `scm_mode` builder is always available; the `cfg(windows)` /
feature check happens inside the crate. (The `--service` flag itself is
*not* `#[cfg(windows)]`-gated — it parses everywhere and is a
documented no-op off Windows, keeping the CLI surface identical across
platforms.) The user closure runs identically under SCM and console
mode — that's the whole point. Service binaries call
`init_service_tracing` (not plain `init_tracing`) and bind the returned
guard as `let _tracing_guard = ...` — a bare `let _ =` drops it
immediately and loses the buffered final log lines.

**No raw std-handle writes on the service path.** Under SCM both std
handles are dead — output there is lost. Diagnostics and errors go
through `tracing` (never `eprintln!`; in console mode the subscriber
writes to stderr anyway, in SCM mode the rolling file gets them). The
one legitimate stdout write — the `bound_addr=` handshake `bdd-infra`'s
port parser reads — must be gated:

```rust
// Console mode only: stdout is a dead handle under the Windows SCM,
// and the only stdout consumer (bdd-infra's port parser) never runs
// services with --service.
if !rusty_photon_service_lifecycle::is_scm_service() {
    println!("Bound Alpaca server bound_addr={local_addr}");
}
```

`is_scm_service()` is a sticky process-global set when SCM mode
engages; it is always `false` in console mode and off Windows, so the
BDD harness contract (port discovery via stdout) is untouched.

No special Bazel wiring is needed in the service's `BUILD.bazel` — just
the plain `//crates/rusty-photon-service-lifecycle` dep. The lifecycle
library target itself enables the `scm` feature via a `crate_features`
`select()` on Windows, so every consumer — direct or transitive (e.g.
via `rusty-photon-driver`) — shares one crate instantiation per
platform. Do not add a per-consumer feature variant: two instantiations
of the crate in one binary's graph stop its types (`ReloadSignal`,
`Shutdown`) from unifying (E0308).

On a run-closure error under SCM, the runner reports `SERVICE_STOPPED`
with a non-zero exit code so SCM (with the installer-configured failure
actions + failure-actions flag) counts the stop as a failure and
restarts the service — the Windows translation of systemd's
`Restart=on-failure` that the serial drivers' eager-validation exits
rely on. Details in the
[crate design](../crates/rusty-photon-service-lifecycle.md).

The worked example is [`services/filemonitor`](../services/filemonitor.md);
its `main.rs` is the canonical reference for what an SCM-enabled
binary looks like.

---

## What the runner does NOT do

These are deliberate boundaries (see the crate design's "Out of scope"
section for the full list):

- **Initialize logging automatically.** The runner never calls
  `init_tracing` / `init_service_tracing` itself. The crate *provides*
  the shared subscriber (`init_service_tracing` — stderr in console mode,
  a rolling file in SCM mode; `RUST_LOG`/fallback filtering, `ErrorLayer`
  for span context in panic reports), but the service binary must call it,
  before the runner, passing its own `--log-level` and `--service` values.
- **Parse CLI arguments.** Services keep `clap`. The runner takes a
  static name and a closure, nothing else.
- **Define what graceful shutdown means for your server.** It just
  produces the *signal*. Composing it into the server's stop path
  (axum's `with_graceful_shutdown`, a `tokio::select!`, etc.) is
  the caller's job.

---

## Common pitfalls

- **Don't install your own signal handler downstream.** If you find
  yourself writing `tokio::signal::ctrl_c().await` inside a service
  that already runs under `ServiceRunner`, you're recreating the bug
  the runner exists to prevent. Use `shutdown.cancelled()` or
  `shutdown.token()` instead. The only exception is one-off CLI tools
  that don't (and shouldn't) use the runner.
- **Don't wrap `ServiceRunner` inside `#[tokio::main]`.** The runner
  builds its own runtime; nesting will either fail at startup or
  produce a confusing two-runtime situation.
- **Don't pass non-`'static` borrows into the closure.** The runner
  requires `Fut: 'static` because the SCM dispatch path type-erases
  the future. `move` ownership into the closure (which is the natural
  shape anyway) and you're fine.
- **`Fut: Send` is not required.** `Runtime::block_on` polls on the
  calling thread; intermediate state inside your closure doesn't need
  `Send` bounds. Only the *closure itself* is `Send + 'static`. The
  closure's *returned error* is the exception: it must be
  `Send + Sync` (the crate's `RunError`), because the runner wraps it
  in a `color_eyre::Report` (ADR-011).

---

## References

- [Crate design](../crates/rusty-photon-service-lifecycle.md) — public
  API, behavior, SCM dispatch internals, testing strategy.
- [Migration plan](../plans/archive/service-lifecycle-unification.md) — how
  the workspace got from twelve divergent shutdown shapes to one,
  closed under issue #294 (with #287 as a side fix in the five
  shared-transport services).
- [`services/filemonitor`](../services/filemonitor.md) — worked example
  for reload + SCM. Its `main.rs` is the canonical reference for an
  SCM-enabled binary.
- [`services/sentinel`](../services/sentinel.md) — worked example for
  the `with_cancellation_token` pattern, where library code already
  takes a token in its builder.

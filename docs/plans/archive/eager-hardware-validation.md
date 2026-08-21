# Plan: service-lifetime transport with split safety / shutdown teardown

> **Status: COMPLETE (archived 2026-05-24; PR #302).** Phases 0a, 0b, and 1-5 landed:
> shared-transport hook split + `start`/`shutdown` API (Phase 0a),
> reconnect supervisor + connection-cell swap that preserves live
> sessions across transient transport loss (Phase 0b), and per-service
> migrations for `star-adventurer-gti`, `dsd-fp2`, `pa-falcon-rotator`,
> `qhy-focuser`, and `ppba-driver` (Phases 1-5) — each unconditionally
> calling `manager.transport().start()` from `ServerBuilder::build()`
> and `manager.transport().shutdown()` from the HTTP-server stop path.
> Phase 6 is this doc update.
>
> An earlier draft introduced a `Config.validate_on_start: bool` flag
> (default `false`) to opt into eager validation per service. That
> flag was removed before merge: there are no pre-existing operator
> configs to be backward-compat with, all CI paths use mock factories
> (which satisfy the handshake), and the only realistic "I want the
> service running without hardware" workflow is local dev — covered
> by the existing `--features mock` builds. Always-validate is the
> opinionated default; operators get an immediate, clear failure if
> the configured port doesn't talk to the right device, rather than a
> deferred failure on the first ASCOM client request.
>
> Originally scoped as "eager hardware validation" (validate-then-close
> at startup, fixing the wrong-device handshake from issue #254). On
> review the scope grew: validation is just one moment in a bigger
> lifecycle redesign that aligns the transport's lifetime with the
> service's lifetime instead of with ASCOM-client refcount. The
> filename is kept for now; consider renaming to `transport-lifecycle.md`
> in a follow-up.
>
> **Deferred follow-ups (documented inline below; not blockers):** on-acquire
> eager reconnect + `reconnect_acquire_timeout`, per-service `Config` fields
> (`reconnect_interval`, `startup_retries`, `startup_retry_backoff`), the
> `--check-device` CLI flag, per-service integration/BDD tests, and the
> optional `transport-lifecycle.md` skill doc.

## Motivation

[Issue #254][issue-254] fixed the immediate Star Adventurer GTi pain
(wrong-device handshake leaked seven mount-specific commands before
the identity check) by reordering the handshake so `:e1` runs first
and strictly validates against a Sky-Watcher mount-type whitelist.
The fix landed in PR #296 and means the wrong-device case now
surfaces a clear, port-quoted `INVALID_OPERATION` to the ASCOM
client on first `Connected = true`.

The follow-up discussion surfaced two deeper questions:

1. **Why is identity-validation gated on the first ASCOM request
   instead of running at service startup?** Classic ASCOM (Windows
   COM in-process driver) has to be lazy because the driver *is*
   the client process. Alpaca looks much more like postgres — a
   daemon that should validate its world at boot and exit non-zero
   on misconfiguration.
2. **Why is the underlying serial / USB port's lifetime tied to
   ASCOM client refcount at all?** In an Alpaca deployment the
   driver process is up only while the device is powered, and vice
   versa: power lifetime ≡ driver lifetime ≡ port lifetime. ASCOM
   clients connect and disconnect on top of a port that's
   continuously open to the device. The "open on first connect,
   close on last disconnect" model conflates two distinct teardown
   moments ("last client walked away, put the device in a safe
   state" vs. "the service is going down, close the port") that
   should be separate.

This plan addresses both by:

- **Splitting the transport lifecycle** from the client refcount.
  The port opens at `transport.start()` and closes at
  `transport.shutdown()`, both called by the service binary. ASCOM
  clients just bump the refcount.
- **Splitting `Hooks::teardown`** into `on_last_disconnect` (refcount
  1→0 safety commands, port stays open) and `shutdown` (final
  cleanup, port closes).
- **Adding a reconnect supervisor** that recovers from transient
  transport loss (USB drop / replug, cable jostle, hub power
  management) without operator or client intervention.

[issue-254]: https://github.com/rusty-photon/rusty-photon/issues/254

## Scope

Five Alpaca driver services, all (or shortly to be) on
[`rusty-photon-shared-transport`][shared-transport-plan]:

| Service | Port | Identity-probe equivalent | Shared-transport status |
|---|---|---|---|
| `dsd-fp2` | 11119 | DSD identity string | on shared transport (first adopter, PR #283) |
| `ppba-driver` | 11112 | Pegasus version query | on shared transport (Phase B, PR #276) |
| `qhy-focuser` | 11113 | JSON identity query | on shared transport (Phase C, PR #280) |
| `pa-falcon-rotator` | 11118 | Pegasus identity query | on shared transport (Phase D, PR #282) |
| `star-adventurer-gti` | 11117 | `:e1` + `MountType` whitelist | on shared transport (Phase E, PR #285); identity-probe landed in PR #296 |

Out of scope:

- `sentinel` (HTTP client polling other Alpaca devices, not a driver).
- `phd2-guider` (PHD2 client, not Alpaca).
- `filemonitor` (FITS file watcher, no hardware-bound transport).

[shared-transport-plan]: ./shared-transport-extraction.md

## The new lifecycle model

```
   ┌──────────┐  start()    ┌──────┐         ┌──────────────┐
   │  Closed  │────────────▶│ Open │────────▶│ Reconnecting │
   └──────────┘             └──────┘  (io     └──────────────┘
        ▲                      │ ▲    error)        │
        │                      │ └─────(success)────┘
        │ shutdown()           │
        └──────────────────────┘
```

State transitions are driven by the **service binary**, not by
ASCOM clients. Clients connect and disconnect on top of a transport
that's already open.

| Event | What happens |
|---|---|
| Service `main` calls `transport.start().await` | Open the port; run `Hooks::handshake` (which encompasses identity validation); spawn `Hooks::while_open` and the reconnect supervisor. On failure: return non-zero `ExitCode` from `main` before binding the Alpaca HTTP server. |
| ASCOM client sends `Connected = true` | `transport.acquire()` returns a `Session` after a fast refcount-bump. No I/O on the happy path. |
| ASCOM client sends `Connected = false` | `Session::close()` decrements the refcount. On 1→0 only: runs `Hooks::on_last_disconnect` to put the device in a safe state (stop tracking, park, turn off heater, …). Port stays open. |
| Service receives SIGTERM | Service's shutdown handler calls `transport.shutdown().await`. Cancels `while_open` + supervisor; runs `Hooks::shutdown` for final cleanup; drops the port. |
| Transport error mid-operation | Transport enters `Reconnecting`. See [§Reconnect mechanism](#reconnect-mechanism). |

The pivotal split: today's `Hooks::teardown` is responsible for
both the safety commands (`star-adventurer-gti` issues `:L1, :L2,
:K1`; others have analogues) **and** the transport teardown (cancel
while-open, drop the port). The new model splits them:

- `Hooks::on_last_disconnect` — fires on every refcount 1→0.
  Per-service safety commands. Port stays open. Idempotent — may
  fire many times during a service's lifetime.
- `Hooks::shutdown` — fires once, from `SharedTransport::shutdown()`
  in the service's shutdown handler. Final cleanup; while_open and
  supervisor are cancelled; port is closed.

Both may run real I/O; both surface errors through their result types.

### Why service-lifetime port ownership

This matches the physical reality of an Alpaca-driver-as-daemon
deployment:

1. **Power lifetime ≡ driver lifetime ≡ port lifetime.** The dome
   gets powered on → the driver process starts (systemd) → the port
   opens. The dome powers off → SIGTERM → port closes. ASCOM clients
   come and go on top of an already-warm transport.
2. **No re-handshake cost between client sessions.** A planetarium
   crashes and reconnects; an automation script disconnects and
   another connects. None of those should re-trigger the seven-byte
   handshake. The handshake belongs to the port's lifecycle, not
   the client's.
3. **Safety teardown still runs at every client disconnect.** A
   client crash or a forgotten disconnect still puts the mount in a
   safe state — the only behaviour worth preserving from the old model.
4. **Polling runs while the device is powered.** The `while_open`
   task (status reads, current-position updates, environmental sensor
   polls) makes sense whenever the device is on, not just while a
   client is asking. Sentinel and the dashboard want continuous
   data; per-client polling was always an awkward fit for what's
   really continuous monitoring.
5. **Transient transport loss becomes a first-class concern.** USB
   serial devices drop and reappear; the current model masks this
   by attempting a fresh acquire on every client request, but a
   client mid-exposure that loses the focuser sees the raw io
   error and has to reconnect itself. The new model owns recovery.

### Trade-offs accepted

| Concern | Old model | New model | Net |
|---|---|---|---|
| External tooling (operator `picocom`s the port) | Possible when no clients connected | Requires stopping the service | Acceptable — the service is the contract |
| Polling while no client is connected | None | Continuous at the configured cadence | Acceptable — small fixed serial traffic; this is what the device is for |
| Recovery from transient transport loss | Implicit via fresh acquire on next client | Explicit supervisor in shared-transport | Explicit-is-better; current model has the "half-broken port until next client" failure mode |
| Boot-time cost | None (lazy-acquire) | One handshake at startup | Acceptable — milliseconds, paid once |
| Shared-transport code complexity | Single teardown hook | Hook split + supervisor | Acceptable — explicit lifecycle is easier to reason about than the conflated one |

## Reconnect mechanism

The reconnect supervisor lives inside `SharedTransport` and runs as
a background tokio task spawned by `start()`. Its job: once the
transport reaches `Open`, keep it there across transient hardware
losses without operator or client intervention.

### Triggers

| Trigger | When | Status |
|---|---|---|
| Periodic | Every `reconnect_interval` (default 5s) while state is `Reconnecting`. | Implemented (Phase 0b). |
| Notify-driven | `Connection::request` fires the supervisor's `Notify` on every `TransportError`, waking the loop immediately rather than waiting for the next periodic tick. | Implemented (Phase 0b). |
| Forced | `SharedTransport::reconnect_now().await` — exposed for test infrastructure and a future operator CLI ("kick reconnect"). | Implemented (Phase 0b). |
| On-acquire | When a client calls `acquire()` and state is `Reconnecting`, attempt one synchronous reconnect (capped by `reconnect_acquire_timeout`, default 2s) before returning. If it succeeds, hand out the session normally; if it doesn't, return `SessionError::Transport(Reconnecting)`. | **Follow-up.** `acquire()` does not short-circuit on `Reconnecting` today; `Session::request` is what reflects the state. Lands together with the `reconnect_acquire_timeout` config plumbing in a follow-up PR. |

### How transport errors enter the supervisor

Two paths reach the supervisor:

1. **`while_open` task** is the canonical detector. Its continuous
   poll loop is the most likely thing to notice the device is gone.
   A transport-error result from any request notifies the supervisor
   (via a `tokio::sync::Notify`) and the task exits its iteration.
2. **Per-request error from `Session::request`** — when a client
   request fails with `TransportError::Io`, `Timeout`, or `Eof`, the
   `Connection` fires the same `Notify`. Many such notifications
   during the brief window before the supervisor flips state to
   `Reconnecting` collapse to a single reconnect attempt.

Codec errors (`TransportError::Framing`, `SessionError::Codec`) do
**not** trigger reconnect — those are protocol mismatches, not
hardware loss, and reconnecting wouldn't fix them.

### Connection swap mechanics

The trick that makes existing `Session<C>` references survive a
reconnect: instead of mutating the existing `Connection<C>` in place,
the slot holds an indirection cell
`ConnectionCell<C> = Arc<RwLock<Arc<Connection<C>>>>`. Both
`SharedTransport`'s `slot` and every `Session<C>` keep an `Arc` clone
of the same cell. The supervisor's reconnect step is:

1. Call `factory.open().await` to get a fresh `FrameTransport`.
2. Wrap it in a freshly constructed `Connection<C>` (a brand-new
   command lock, no shared state with the dead one).
3. Run `Hooks::handshake` against the new `Connection` in isolation —
   it owns its own command lock, no contention with live sessions
   (which are still pointing at the old, dead cell value).
4. Cancel the previous `Hooks::while_open` task (with a bounded
   timeout join, then abort) before installing the new connection,
   so the dying poll loop's in-flight requests don't race the swap.
5. Take the cell's `write` lock briefly and replace the inner
   `Arc<Connection<C>>` with the new one (an atomic pointer swap
   from any reader's perspective).
6. Respawn `Hooks::while_open` against the fresh connection.

`Session::request` reads the cell via `cell.read().await.clone()` on
every call, so live `Session<C>`s automatically pick up the new
connection on their next request — no client-visible recreation is
needed. The previous `Arc<Connection<C>>` drops when the last in-flight
reader releases its clone, taking the dead `FrameTransport` with it.

### Backoff

Default schedule: every 5 seconds, no exponential growth. Reasoning:
a 5-second cadence is fast enough that a brief unplug/replug cycle
recovers within two attempts and slow enough that a dead device
doesn't spam syslog. Configurable per service via `reconnect_interval`.

### Reporting

While in `Reconnecting`:

- `SharedTransport::is_available()` returns `false`.
- `Session::request()` returns `SessionError::Transport(Reconnecting)`
  — a labelled variant so per-service error mapping can render
  "device is reconnecting" instead of a generic io error.
- ASCOM `Connected` continues to reflect the slot's session presence
  (clients haven't called `set_connected(false)`); operations fail
  until the supervisor recovers. See open question #1 on whether
  `Connected` should track availability instead.

A future enhancement: emit `reconnect_started` / `reconnect_succeeded`
/ `reconnect_failed` events so `sentinel` and the dashboard can
surface "focuser is reconnecting" instead of generic request failures.

## Shared-transport API changes

```rust
// crates/rusty-photon-shared-transport/src/lib.rs
impl<C: Codec> SharedTransport<C> {
    /// Open the physical port, run `Hooks::handshake` (identity
    /// validation), and spawn `Hooks::while_open` plus the reconnect
    /// supervisor. Called once at service startup, before the Alpaca
    /// HTTP server binds. On error returns the handshake's
    /// `SessionError<C::Error>` so `main` can map it to a non-zero
    /// `ExitCode`.
    pub async fn start(&self) -> Result<(), SessionError<C::Error>>;

    /// Final teardown: cancel `while_open` + reconnect supervisor;
    /// run `Hooks::shutdown`; drop the port. Called from the
    /// service's SIGTERM handler.
    pub async fn shutdown(&self) -> Result<(), TransportError>;

    /// Hand out a `Session`. Fast on the happy path: refcount++
    /// and clone the existing connection `Arc`. No I/O on the 0→1
    /// client transition (port is already open). On `Reconnecting`,
    /// see the on-acquire trigger above.
    pub async fn acquire(&self) -> Result<Session<C>, SessionError<C::Error>>;

    /// Trigger an immediate reconnect attempt and await its result.
    pub async fn reconnect_now(&self) -> Result<(), SessionError<C::Error>>;
}

/// Session::close decrements the refcount. On 1→0, runs
/// `Hooks::on_last_disconnect`. Port stays open.
impl<C: Codec> Session<C> {
    pub async fn close(self) -> Result<(), TransportError>;
}

/// Hooks split: on_last_disconnect vs shutdown.
pub struct Hooks<C: Codec> {
    pub handshake: HandshakeFn<C>,
    /// Runs on every refcount 1→0. Port stays open. Idempotent.
    pub on_last_disconnect: OnLastDisconnectFn<C>,
    /// Runs once on `SharedTransport::shutdown()`. Port closes after.
    pub shutdown: ShutdownFn<C>,
    /// Lifecycle tied to transport `Open`, not client refcount > 0.
    pub while_open: Option<WhileOpenFn<C>>,
}
```

### Rollout compatibility (implemented)

Phase 0a renamed `Hooks.teardown` to `Hooks.on_last_disconnect`
and added a new `Hooks.shutdown` field. The plan originally
proposed a `Hooks::legacy_teardown(t)` constructor as a compile-time
shim, but in practice every service had a no-op or trivial teardown
that was easier to update in-place than to wrap behind a shim — so
the shim was skipped, each service got a manual two-line update to
its `Hooks { ... }` literal, and the `LazyAcquire` runtime
semantics ensure `on_last_disconnect` keeps firing on every 1→0
close exactly as the old single hook did. Migrated services
additionally got a `shutdown` body (no-op for the four no-op
services; the same `:L1, :L2, :K1` safety sequence as
`on_last_disconnect` for `star-adventurer-gti`).

## Per-service work

| Service | Audit | Likely change |
|---|---|---|
| `star-adventurer-gti` | Wrong-device probe landed in PR #296. Existing teardown is `:L1, :L2, :K1`. | Move teardown to `on_last_disconnect`; add `Hooks::shutdown` (likely the same commands here); wire `start`/`shutdown` in `main`. |
| `dsd-fp2` | Confirm handshake checks an identity string and rejects on mismatch. | If not: port the `WrongDevice` pattern (codec error + diagnostic + service error + ASCOM mapping); split teardown; wire start/shutdown. |
| `ppba-driver` | Pegasus PPBA has a version/identity query — confirm handshake rejects non-PPBA replies. | Same as above. |
| `pa-falcon-rotator` | Pegasus Falcon shares Pegasus identity shape with PPBA. | Same; consider a shared `pegasus-protocol` crate (track separately). |
| `qhy-focuser` | QHY's JSON identity probe — confirm rejection on non-QHY JSON. | Same. |

Each service's `main.rs` grows two calls:

```rust
manager
    .transport()
    .start()
    .await
    .map_err(|e| {
        error!(error = %ServiceError::from(e), "hardware startup failed");
        ExitCode::from(2)
    })?;

// … in the SIGTERM handler …
manager.transport().shutdown().await.ok();
```

`rusty-photon-service-lifecycle` will probably grow a hook to thread
`start`/`shutdown` through automatically — track separately.

## Config surface (per service)

### What landed (Phases 1-5)

No per-service `Config` fields. Each migrated service's
`ServerBuilder::build()` unconditionally calls
`manager.transport().start()` after constructing the manager and
before binding the HTTP listener; the resulting `SessionError` (if
any) propagates out of `build()` into `main`, which returns a
non-zero `ExitCode`. There is no opt-out — see the status banner at
the top for the rationale.

The reconnect supervisor's cadence uses the fixed default
`DEFAULT_RECONNECT_INTERVAL = 5s` baked into shared-transport;
operators wanting a different cadence can call
`SharedTransport::set_reconnect_interval()` from a service-specific
bootstrap path, but no `Config` field exposes this yet.

### Follow-ups deferred to a later PR

| Field | Purpose | Status |
|---|---|---|
| `reconnect_interval: Duration` | Override the supervisor's periodic retry cadence per-service from `Config` (instead of calling `set_reconnect_interval` manually). | Not implemented. |
| `reconnect_acquire_timeout: Duration` | Time `acquire()` will wait for the on-acquire eager-reconnect path before returning `Reconnecting`. | Not implemented (the on-acquire path itself is a follow-up — see the [§Triggers](#triggers) table). |
| `startup_retries: u32` + `startup_retry_backoff: Duration` | Retry-loop wrapping `transport.start()` for the dome-power-on-same-circuit case (mount comes up seconds after the daemon). | Not implemented. Operators relying on this today need to wrap their systemd unit with `Restart=on-failure` + `RestartSec=` instead. |

## CLI surface (per service)

**Not implemented in this PR — follow-up.** The original plan
proposed a `--check-device` flag that forces a one-shot validation
pass and exits:

```bash
star-adventurer-gti --config /etc/dome.json --check-device
# exit 0 → hardware verified
# exit 2 → wrong device or transient failure
```

Useful for "verify before service install" workflows and CI
hardware-attached smoke tests. The implementation maps to
`transport.start()` + `transport.shutdown()` + exit, but it needs
per-service `Args` wiring (clap derive in each `main.rs`) and a
branch that bypasses `ServiceRunner` and the HTTP listener bind.
Five services' worth of mechanical wiring; tracked as a follow-up.

Operators today can approximate the check by running the service
with a config that points at the device they want to verify — the
binary will exit non-zero on handshake failure. Less ergonomic than
a dedicated flag (it still binds the HTTP listener on success), but
functionally equivalent for the "does the configured port talk to
the right device" question.

## Test strategy

### Landed in this PR

- **Shared-transport `tests/lifecycle.rs`** (Phase 0a, 12 tests):
  - `start()` runs handshake; `shutdown()` runs `Hooks::shutdown`;
    refcount=0 in between doesn't close the port.
  - `Session::close()` on 1→0 runs `on_last_disconnect` exactly once
    but doesn't close the port (in `ServiceLifetime` mode).
  - `LazyAcquire` mode preservation: bit-for-bit unchanged from
    pre-Phase-0a.
  - `while_open` task lifecycle across all mode transitions.
- **Shared-transport `tests/reconnect.rs`** (Phase 0b, 7 tests):
  - `reconnect_now()` opens a fresh transport, runs handshake,
    swaps the cell — happy path.
  - **Live `Session<C>` references survive a reconnect via cell
    swap** (the headline contract from the
    [§Connection swap mechanics](#connection-swap-mechanics) section).
  - Refcount invariance across reconnect; supervisor cancellation
    on `shutdown()`.
  - Reconnect-failure case: stays in `Reconnecting` until next
    successful retry; `Session::request` short-circuits to
    `TransportError::Reconnecting` while the supervisor is in
    that state.
  - **Codec errors do not trigger reconnect** (the contract
    enforced in `Connection::request`'s signal-fire logic).

### Follow-ups

- **On-acquire eager reconnect test**: documented in the
  `tests/reconnect.rs` file header. Lands together with the
  `reconnect_acquire_timeout` config plumbing in a follow-up PR.
- **Per-service integration tests** ("wrong-device mock → service
  `main` returns non-zero"; "session round-trip survives a simulated
  transport-drop"): not landed; worth adding per service in
  follow-ups.
- **BDD scenarios** ("service refuses to start when configured port
  targets the wrong device"; "session resumes after a transient
  transport drop"): not landed; need a wrong-device mock factory
  per service first.
- **CI nightly hardware-attached run** ([pi5 nightly][pi5]):
  unchanged today. Production hardware paths now go through
  `transport.start()` unconditionally on boot — the nightly run
  already exercises that path on every restart.

[pi5]: ../../docs/operations/pi-nightly-runner.md

## Failure-mode matrix

| Scenario | Behaviour (current implementation) |
|---|---|
| Device powered off at boot | `start()` fails. Exits 2 immediately (no `startup_retries` plumbing yet — that's a follow-up; operators wrap the systemd unit with `Restart=on-failure` for now). Orchestrator restarts the binary. |
| Device powered on, wrong device | `start()` fails with `WrongDevice`. Exits 2 immediately with the diagnostic. |
| Device powered on, transient handshake failure (CRC, dropped byte) | `start()` fails; same restart-via-orchestrator path as the powered-off case. Per-attempt retries inside `start()` would need the `startup_retries` follow-up. |
| Device powered on, correct device | `start()` succeeds; clients connect on top of a warm transport. |
| Transient transport loss mid-session | Supervisor enters `Reconnecting`. Periodic retry every `DEFAULT_RECONNECT_INTERVAL` (5s, override via `SharedTransport::set_reconnect_interval`). Sessions stay alive but requests fail with `TransportError::Reconnecting` until recovery. On success, sessions resume on the new connection via the cell swap. |
| Permanent device removal | Supervisor stays in `Reconnecting` forever. Operator notices via sentinel / dashboard. (Future enhancement: configurable max-reconnect-attempts before the service exits 3 and lets the orchestrator decide.) |

## Phasing

Mirror the [shared-transport-extraction precedent][shared-transport-plan]
(PR-per-phase):

1. **Phase 0a** — `rusty-photon-shared-transport`: hook split
   (`on_last_disconnect` + `shutdown`), `start()` / `shutdown()`
   API. One PR. (No `legacy_teardown` shim ended up needed — see
   [§Rollout compatibility](#rollout-compatibility-implemented).)
2. **Phase 0b** — reconnect supervisor + connection-swap mechanics.
   One PR. Lands behind the API from 0a; services that don't
   override `reconnect_interval` get the default behaviour.
3. **Phase 1** — `star-adventurer-gti`: migrate to explicit hooks;
   wire `start`/`shutdown`; add BDD coverage. Already has the
   `WrongDevice` plumbing from PR #296.
4. **Phase 2** — `dsd-fp2`: audit handshake; migrate.
5. **Phase 3** — `pa-falcon-rotator`: audit; migrate.
6. **Phase 4** — `qhy-focuser`: audit; migrate.
7. **Phase 5** — `ppba-driver`: audit; migrate. Possibly bundle with
   Phase 3 if a `pegasus-protocol` consolidation lands first.
8. **Phase 6** (optional) — workspace docs:
   `docs/skills/transport-lifecycle.md` codifying the pattern.
   Updating this plan-doc itself with the implemented-status
   headers is the minimum Phase 6 deliverable that landed; a
   stand-alone skill doc remains as a follow-up enhancement for a
   future contributor.

Each phase was independently mergeable up through Phase 5; the
flag-removal cleanup that followed makes `ServiceLifetime` mode the
unconditional shape for all five services and removes the per-service
opt-out.

## Open questions worth surfacing before starting

1. **ASCOM `Connected` semantics during `Reconnecting`.** When the
   transport is in `Reconnecting` but the device's session slot is
   `Some`, should `connected()` return `true` (client intent) or
   `false` (current availability)? Current code computes
   `slot.is_some() && transport.is_available()`, which would flip
   to `false` during reconnect. Argument for keeping it that way:
   matches today's observable behaviour. Argument for `true`: a
   client asking "am I connected" wants to know if their session
   handle is still valid, not whether the wire is healthy right
   this instant. Default proposal: keep current behaviour
   (`slot && is_available`) — it's the least surprising delta.
2. **`set_connected(false)` while `Reconnecting`.** Does the
   supervisor stop when the last client disconnects? No — the
   supervisor is service-scoped, not client-scoped. The 1→0
   transition runs `on_last_disconnect` (which itself will fail
   because the transport is down; failure is logged but not
   propagated), and the supervisor keeps trying.
3. **Issue #288 (lock-drop around slow I/O on `set_connected`).**
   **Resolved by Phases 0a-0b + flag removal.** The new lifecycle
   makes #288's original concern moot: `acquire()` is a fast
   refcount-bump (no I/O), and `Session::close()` on 1→0 is also
   fast in `ServiceLifetime` mode (the port stays open;
   `on_last_disconnect` runs but is per-service short). The
   connection-cell swap in Phase 0b additionally means a transient
   transport-loss event doesn't block any device's read lock —
   sessions short-circuit to `TransportError::Reconnecting` while
   the supervisor recovers. With the `validate_on_start` flag
   removed, every service runs in `ServiceLifetime` mode in
   production, so the close-path I/O regression #288 originally
   flagged is structurally unreachable.
4. **systemd unit semantics.** Should we ship a sample `.service`
   file in `deploy/` with `Restart=on-failure` + `RestartSec=`
   matched to `startup_retry_backoff`? Otherwise operators
   reinvent the retry budget at the orchestrator layer.
5. **Discovery responder during startup.** Bind discovery after
   `transport.start()` succeeds. Don't advertise a device we
   haven't confirmed.
6. **Polling while no client is connected.** Confirm each
   service's poll interval is fine when the device is idle. Likely
   yes for all five (status reads are a few bytes/sec); flag any
   that have a measurable cost.
7. **Bundling Pegasus identity in a crate.** `ppba-driver` and
   `pa-falcon-rotator` both speak the Pegasus protocol family.
   Their identity probes could share a `pegasus-protocol` crate
   (analogous to `skywatcher-motor-protocol`). Track separately
   if Phase 3 / Phase 5 audits surface enough duplication.
8. **Permanent reconnect failure → service exit.** Should the
   supervisor have an upper bound on consecutive failed reconnects
   before declaring the device dead and exiting non-zero? Argument
   for: clean orchestrator handoff (systemd restart loop, possibly
   with hardware-power-cycle automation). Argument against:
   transient losses can be long (overnight cable issue), and
   exiting throws away in-flight client state. Default proposal:
   no upper bound; future opt-in `max_reconnect_attempts` config
   if operators ask for it.

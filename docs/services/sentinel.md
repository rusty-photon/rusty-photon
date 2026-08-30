# Sentinel Service

## Overview

Sentinel is an observatory monitoring and notification service. It polls ASCOM Alpaca devices via their HTTP API, detects state transitions (safe/unsafe), and sends notifications through configurable channels. It supervises the health of the installed rusty-photon services — discovered from the platform service manager, not configured (see [Service Discovery](#service-discovery)) — with periodic `GET` health probes and autonomous restart ([Service Health Supervision](#service-health-supervision)), and provides a web dashboard for real-time status viewing.

Unlike other services in this workspace, sentinel is **not** an ASCOM Alpaca server — it is a client/consumer that monitors other ASCOM devices.

## Architecture

```
Config (JSON)
     |
     v
  main.rs --- CLI (clap) + tracing
     |
     v
  Engine --- orchestrator: owns monitors, notifiers, shared state
   +--+--+
   |     |
   v     v
Monitor trait          Notifier trait
   |                      |
   v                      v
AlpacaSafetyMonitor   PushoverNotifier
(reqwest GET/PUT)     (reqwest POST)

SharedState (Arc<RwLock>) <-- Engine updates
     |
     v
Dashboard (axum + hand-rolled HTML) --- web UI w/ JS polling
```

### Key Traits

- **`Monitor`** — `poll() -> MonitorState`, `connect()`, `disconnect()`, `polling_interval() -> Duration`. A **pull-based** monitor the engine polls on a fixed interval. First implementation: `AlpacaSafetyMonitor`.
- **`EventMonitor`** — `name()`, `run(cancel)`. A **self-driving** monitor task that owns its own lifecycle — a long-lived connection it reacts to, or a poll loop it paces itself — and runs until the cancellation token fires. Added because some monitors cannot be expressed as the engine-paced `poll() -> State` shape; the engine spawns a parallel task per `EventMonitor` alongside the per-`Monitor` poll loops. Implementations: `OperationDeadlineMonitor` (see [Operation Watchdog](#operation-watchdog)) and `ServiceHealthSupervisor` (see [Service Health Supervision](#service-health-supervision)).
- **`Notifier`** — `notify(notification)`. First implementation: `PushoverNotifier`. The watchdog reuses this dispatch path — an expiry or liveness escalation is just another notification.
- **`Corrective`** / **`HealthChecker`** / **`Aborter`** / **`Restarter`** — the watchdog's corrective-action ladder for `abort_then_restart` operations. `CorrectiveLadder` composes the three rung traits (health → abort → restart) behind the single `Corrective::run` seam the watchdog calls; HTTP and shell default impls live in `corrective.rs`. See [Operation Watchdog](#operation-watchdog). `Restarter` (run a shell command bounded by a budget, `Ok` iff it exits 0 in time) is also the seam behind the [Service Restart API](#service-restart-api): the REST endpoint runs both the derived restart command and the derived recovery poll through it, so tests inject a recording stub.
- **`ServiceManager`** — the [service discovery](#service-discovery) seam: enumerate installed `rusty-photon-*` units with run states, derive the restart/recovery commands. Implementations: systemd (Linux), SCM (Windows), Homebrew services (macOS), and the directory-backed test stub.
- **`HttpClient`** — wraps `reqwest` for testability (mockall in tests). Used by monitors, notifiers, and the corrective health-check / abort rungs.

### Dependency Injection

`Config` provides `build_monitors()` and `build_notifiers()` factory methods (in `lib.rs`) that map config enums to concrete implementations. `SentinelBuilder` uses these by default.

For custom monitors/notifiers (e.g. in an astrophotography app), use `with_monitors()` / `with_notifiers()` on the builder to inject pre-built instances, bypassing the config factories entirely.

### State Flow

1. Engine starts, connects each monitor (PUT `connected=true`)
2. Engine spawns a tokio polling task per monitor at configured interval
3. Each poll: GET `issafe`, compare with stored state, if transition matches a configured rule, dispatch to notifiers
4. SharedState updated on every poll, dashboard reads from it
5. On shutdown: disconnect monitors (PUT `connected=false`)

## Configuration

Configuration is loaded from a JSON file. All sections are optional with sensible defaults.
Every section (the top-level `Config` plus each nested block —
`MonitorConfig`, `NotifierConfig`, `TransitionConfig`, `DashboardConfig`,
`ServerConfig`, `OperationWatchdogConfig`, `OperationPolicy`)
rejects unknown keys at deserialize (`deny_unknown_fields`), so a typo or a
key removed by a schema change fails loudly at load instead of being silently
ignored. In particular a config still carrying the retired `services` map
(deleted when sentinel moved to [service discovery](#service-discovery))
fails at load with the offending key named.

See `examples/config.json` for a complete example.

### Server and dashboard

The dashboard listener is configured by the top-level `server` block;
`dashboard` keeps only what the dashboard itself does (whether it is served,
how much notification history it keeps):

```json
{
  "server": {
    "port": 11114,
    "bind_address": "0.0.0.0"
  },
  "dashboard": {
    "enabled": true,
    "history_size": 100
  }
}
```

The `server` block is the shared `ServerConfig` from
`crates/rusty-photon-server-config` (see ADR-016): `port` (default `11114`),
`bind_address` (default `0.0.0.0`), and optional `tls`/`auth`. Absent
`tls`/`auth` means plain, unauthenticated HTTP.

### The observatory probe credential

Two optional top-level keys wire sentinel into a doctor-provisioned rig
(on a self-signed install `doctor --fix` writes both; on an ACME install
it writes only `service_auth` — a pin would break public trust, below.
See
[`doctor.md` §Provisioning](doctor.md#provisioning--tls-and-the-observatory-credential-d6a)):

```json
{
  "ca_cert": "/var/lib/rusty-photon/.config/rusty-photon/pki/ca.pem",
  "service_auth": { "username": "observatory", "password": "<plaintext>" }
}
```

`ca_cert` pins the CA the HTTP clients trust for TLS-enabled peers. The
pin **replaces** the platform trust roots (ADR-002), so it belongs only on
a self-signed install; when it is absent the clients verify against the
platform roots — which is what verifies the publicly-trusted certificates
of an ACME install, where the pin must stay unset. `service_auth` is the
observatory credential's plaintext copy: when set, the health-supervision
probes send it as HTTP Basic auth over a **verified** TLS connection —
verified against `ca_cert` when pinned, against the platform roots
otherwise (credentials never ride an unverified connection); when absent,
probes are unauthenticated and skip certificate verification (a challenge
still proves aliveness). A `ca_cert` naming a missing or unreadable file
is a misconfiguration: sentinel logs an error and probes unauthenticated
with verification off — withholding the credential — until the pin is
fixed or removed. `doctor auth rotate` overwrites the password in place.

### The probe-host override (`probe_domain`)

One optional top-level key overrides the host every
[derived probe URL](#deriving-the-health-probe-url) dials:

```json
{
  "probe_domain": "rig.example.com"
}
```

Unset (the default), probe hosts come from the supervised service's
`bind_address` — `localhost` for a wildcard bind, the literal address
otherwise. That derivation breaks the moment services serve an ACME
wildcard certificate (`*.<host>.<domain>`): a public CA issues DNS SANs
only — never `localhost` or a private IP — so every https probe would fail
hostname verification and report a healthy fleet as down. With
`probe_domain` set, every derived URL's host is `<service>.<probe_domain>`
(`https://qhy-camera.rig.example.com:11121/health`), matching the wildcard
SAN. The names must resolve to the local host (hosts file or local
resolver) — the same requirement the ACME flip imposes on every other
client, because sentinel stays same-host-bound either way. The value must
be a bare DNS domain of letter/digit/hyphen labels: a scheme, port, path,
whitespace, empty label, or any character a certificate's DNS SAN could
never carry fails config load with the field named.

### Service discovery

Sentinel has **no configured service registry**. It discovers the services it
supervises from the platform service manager: at startup and every 60 s
thereafter it enumerates the installed `rusty-photon-*` service units
(excluding its own, and excluding scheduled-job units — currently
`rusty-photon-renew`, the TLS renewal oneshot its own package ships:
supervising a job would restart-loop a failed run, and a dashboard row
for it would be noise) and derives everything the retired `services` map
used to spell out. The rule (ADR-016 decision 8) is not "static vs. dynamic" but
*whether the source of truth can be down when you need it*: asking a dead
driver how to restart itself fails that test; asking the service manager —
which is alive precisely when the driver is not — passes it. Discovery is
re-run on a fixed cadence so a newly installed package is picked up, and a
removed one dropped, without restarting sentinel.

| Fact | Derivation |
|---|---|
| which services exist | enumerate `rusty-photon-*` service units (below) |
| service name | unit name minus the `rusty-photon-` prefix (`rusty-photon-dsd-fp2` → `dsd-fp2`) |
| restart command | from the unit name: `systemctl restart <unit>` / `Restart-Service <unit>` / `brew services restart <unit>` |
| recovery check | `systemctl is-active <unit>` / `sc query <unit>` reports `RUNNING` / *(macOS: skipped)* |
| health probe URL | scheme + port from the service's own `<svc>.json` (below); path from the service's probe class |
| restart budget, poll cadence, thresholds, backoff | constants — see [Service Health Supervision](#service-health-supervision) |

Sentinel is same-host-bound by definition (it shells out to the service
manager), so every derived URL targets the local host — by address, or by a
locally-resolving `<service>.<probe_domain>` name when the
[probe-host override](#the-probe-host-override-probe_domain) is set.
Processes started by hand (`cargo run`) are not under the service manager
and are therefore not discovered or supervised.

#### Enumeration and run states

| Platform | Enumeration | State source |
|---|---|---|
| Linux (deb/rpm, system units) | `systemctl list-unit-files 'rusty-photon-*.service'` | `systemctl show <unit>` (`ActiveState`, `SubState`, `UnitFileState`, `ConditionResult`) |
| Windows (MSI, SCM) | SCM services named `rusty-photon-*` | service status + start type |
| macOS (Homebrew) | `brew services list` filtered to `rusty-photon-*` | brew service status |

Each discovered service is classified by run state, which decides what
supervision does with it:

| Run state | Meaning | Supervision |
|---|---|---|
| `running` | unit active (or activating) | health-probed; restarted autonomously on hang |
| `failed` | unit failed — the OS supervisor's `Restart=on-failure` gave up | restarted autonomously (sentinel never gives up) |
| `inert` | installed and enabled, but a start condition is unmet — the `ConditionPathExists` config gate of plate-solver, sky-survey-camera, calibrator-flats, session-runner, and polar-align | displayed only. A config-gated service that has never been given a config is deliberate, not broken; restart-looping it would be pure notification spam (the doctor flags it instead) |
| `stopped` | inactive without a failed state — the operator stopped it | displayed only. An operator-stopped service stays stopped |
| `disabled` | unit file disabled or masked | displayed only |

Only `running` and `failed` services are autonomously restarted. The
[manual REST restart](#service-restart-api) is exempt from this classification
— it is the operator's recovery hammer and restarts any discovered service.

#### Deriving the health probe URL

Sentinel reads the supervised service's **own config file** — `<svc>.json` in
the directory of sentinel's own config file (on a packaged install every
service resolves the same platform config directory —
`/var/lib/rusty-photon/.config/rusty-photon/` on Linux,
`%PROGRAMDATA%\rusty-photon` on Windows, `~/Library/Application
Support/rusty-photon/` on macOS; in tests a scratch dir holds sibling
files). Since D1 every service's `server` block uses the shared
`rusty-photon-server-config` shapes, so one permissive parse recovers what the
probe needs: `port` (required), and whether `tls` is configured (scheme
`https` vs `http`). The parse is deliberately **permissive** — only the
`server` block is read and unknown fields are tolerated (ADR-016 decision 7:
strict parsing is for a service's own config; a cross-binary read from a
possibly newer or older build must degrade, not refuse). The probe host is
`localhost` (the service's `bind_address` when it names a specific address) —
unless the [`probe_domain` override](#the-probe-host-override-probe_domain)
is set, which replaces it with `<service>.<probe_domain>` in every derived
URL (the health probe and the watchdog ladder's Alpaca base alike).
The probe path follows the service's **probe class**:

- **Alpaca drivers** answer `GET {base}/management/v1/configureddevices` — no
  device number needed, so no device knowledge leaks into sentinel.
- **Non-Alpaca services** (`rp`, `plate-solver`, `session-runner`,
  `calibrator-flats`, `polar-align`, `phd2-guider`, `ui-htmx`) answer
  `GET {base}/health`.
  These are exactly the services that define a `/health` route; the Alpaca
  drivers have none, by design. The set is a compile-time constant; a new
  non-Alpaca service must be added to it (a unit test asserts every listed
  name exists under `services/*/pkg`).

A missing or unreadable `<svc>.json` means no probe URL can be derived: the
service's health reports `unknown`, nothing is restarted because of it, and
the read is retried on every discovery cycle. Every supervisable service
self-creates its default config on first start (the shared
`rusty_photon_config::resolve_and_init` bootstrap), so a persistent `unknown`
on a `running` service points at the file being unreadable — or at
`session-runner`, whose config is deliberately operator-provided (it has no
usable defaults) and therefore only exists once the operator has written it.

#### The test seam (`SENTINEL_SERVICE_MANAGER_DIR`)

Tests cannot install systemd units, so when the environment variable
`SENTINEL_SERVICE_MANAGER_DIR` is set sentinel swaps the platform backend for
a **directory-backed stub** (all platforms, no shell):

- enumeration reads `<dir>/units.txt` — one `<unit> <run-state>` pair per
  line;
- a restart appends the unit name to `<dir>/restarts.log` and succeeds,
  unless `<dir>/restart-fail-<unit>` exists (then it fails);
- the recovery check passes iff the unit's `units.txt` state is `running`.

The sentinel BDD suites and the operation-watchdog e2e harness drive
supervision through this seam (mutating `units.txt` mid-scenario to model
crashes and operator stops, asserting `restarts.log` to prove a restart ran).
It is a test seam, not an operator feature — production installs must let
discovery ask the real service manager.

**Restart privileges.** On a packaged Linux install the unit runs sentinel as
the unprivileged `rusty-photon` user with `NoNewPrivileges=yes`, so a `sudo`
prefix could never work (setuid is blocked), and plain `systemctl restart` is
only authorized because the sentinel package ships a scoped polkit rule
(`/usr/share/polkit-1/rules.d/50-rusty-photon-sentinel.rules`)
letting the `rusty-photon` user restart `rusty-photon-*` units — and nothing
else: other verbs (start, stop, enable) and non-prefixed units such as
`ssh.service` still require the usual interactive authorization. The rule's
scope and the discovery scope are the same set. The rule needs the
JavaScript-rules polkitd (any current Debian or Fedora ships it).
On Windows the MSI installs every service — sentinel included — under
`LocalSystem`, which may restart services, so `Restart-Service` needs no
analogous grant; on macOS `brew services` run as the operator's own user, so
no privilege boundary is crossed.

### Monitor Types

- `alpaca_safety_monitor` — Polls an ASCOM Alpaca SafetyMonitor device. Supports optional `auth` section with `username` and `password` (plaintext) for connecting to auth-enabled services. See [ADR-003](../decisions/003-authentication-for-device-access.md).

In addition to the polled monitors above, the optional `operation_watchdog` block configures the push-based [Operation Watchdog](#operation-watchdog), which subscribes to an rp event stream rather than polling a device.

### Notifier Types

- `pushover` — Sends push notifications via the Pushover API. Accepts an
  optional `api_url` field that overrides the endpoint
  (`https://api.pushover.net/1/messages.json` by default); set it to point at a
  self-hosted Pushover-compatible relay, or at a local stub in tests.

### Environment Variable Overrides

Pushover credentials can be provided (or overridden) via environment variables so that secrets do not need to be committed to the config file:

| Variable | Overrides |
|----------|-----------|
| `PUSHOVER_API_TOKEN` | `api_token` in Pushover notifier config |
| `PUSHOVER_USER_KEY` | `user_key` in Pushover notifier config |

When set and non-empty, environment variables take precedence over JSON config values. When using environment variables exclusively, the credentials can be omitted from the JSON config entirely.

After resolution, sentinel returns a configuration error if either field is still empty.

**Usage with 1Password CLI:**

Create a `.env` file (already in `.gitignore`) with `op://` secret references:

```
PUSHOVER_API_TOKEN=op://Personal/Pushover/add more/sentinel app key
PUSHOVER_USER_KEY=op://Personal/Pushover/add more/user key
```

Then run sentinel with `op run` to inject the resolved values:

```bash
op run --env-file .env -- cargo run -p sentinel -- -c services/sentinel/examples/config.json -l debug
```

### Transition Rules

Transitions define when notifications should be sent. Each rule specifies:
- Which monitor to watch (`monitor_name`)
- Which direction of change (`safe_to_unsafe`, `unsafe_to_safe`, or `both`)
- Which notifiers to use
- A message template with `%monitor_name%` and `%new_state%` placeholders
- Optional priority and sound overrides

Empty transitions config means no notifications are sent.

### Authentication

The dashboard supports optional HTTP Basic Auth via the top-level
`server.auth` config section. Monitor connections to auth-enabled Alpaca services use
per-monitor `auth` credentials (plaintext password, `chmod 600` recommended).
See [ADR-003](../decisions/003-authentication-for-device-access.md).

```json
{
  "monitors": [{
    "type": "alpaca_safety_monitor",
    "name": "Roof Sensor",
    "host": "localhost", "port": 11111, "device_number": 0,
    "scheme": "https",
    "auth": { "username": "observatory", "password": "secret" }
  }]
}
```

## Operation Watchdog

The operation watchdog is sentinel's second monitoring loop. Where
`AlpacaSafetyMonitor` answers "is the sky safe?", the watchdog answers
"is the running operation making progress, and is rp itself alive?".
It is the Sentinel half of the two-loop supervision design described in
[`docs/services/rp.md` §Sentinel Watchdog Integration](rp.md#sentinel-watchdog-integration)
and the
[predictive-deadlines plan](../plans/archive/predictive-deadlines-and-watchdog.md);
rp is the inner loop (every blocking operation emits a `*_started` event
carrying a predicted and a maximum duration, then a `*_complete` /
`*_failed`), and the watchdog is the outer loop that tracks those
deadlines independently and escalates when one is missed.

It is implemented as an `EventMonitor` (`OperationDeadlineMonitor`) that
subscribes to rp's Server-Sent-Events stream
(`GET {rp_url}/api/events/subscribe`, see
[rp.md §Real-Time Stream](rp.md#real-time-stream)) and reacts to each
event as it arrives. It is entirely optional: with no `operation_watchdog`
block in the config, sentinel behaves exactly as before (safety polling
only).

### What it tracks

Every event on the stream carries an envelope with an `event` type (e.g.
`"slew_started"`), an `event_seq` (the monotonic SSE id / replay cursor),
an `operation_id` (shared across one operation's lifecycle events), and —
on `*_started` events for operations with predictive deadlines — a
`max_duration_ms`. The watchdog keys open operations by `operation_id`:

| Event suffix | Action |
|---|---|
| `*_started` | Begin tracking this `operation_id`. If the envelope carries `max_duration_ms`, arm an expiry timer for `max_duration_ms + buffer` (the **buffer** covers wire latency and gives rp's own internal deadline a chance to fire first). If it does not (operations not yet on predictive deadlines, e.g. `plate_solve`), track it open with no timer — it clears only on completion or via the liveness pulse. |
| `*_complete` / `*_failed` | Stop tracking this `operation_id`; cancel its timer. The operation finished within its deadline — no escalation. |
| timer expiry (no matching completion) | **Escalate** per the operation family's `on_expiry` policy. |

The operation **family** (the key used to look up `buffer` / `on_expiry`)
is the event name with its `_started` / `_complete` / `_failed` suffix
stripped: `slew_started` → `slew`, `move_focuser_complete` →
`move_focuser`, `centering_started` → `centering`. A family with no
configured entry uses the watchdog's `default_buffer` and a `notify_only`
policy.

The timer is armed from the moment the `*_started` frame is **received**,
not from the envelope's `started_at` wall-clock — this needs no clock
synchronisation between hosts, and on reconnect (below) a replayed
`*_started` that is already overdue fires almost immediately, which is the
correct conservative behaviour.

### Liveness pulse (stream disconnect)

The stream doubles as a heartbeat on rp. If the connection drops — rp
crashed, wedged, or restarted — the watchdog:

1. Reconnects with `Last-Event-ID` set to the highest `event_seq` it has
   seen, so rp replays everything buffered since (see
   [rp.md §Real-Time Stream](rp.md#real-time-stream) — rp keeps a 512-event
   history ring). Reconnect uses a fixed `reconnect_backoff` delay, up to
   `reconnect_max_attempts` times.
2. If rp answers a reconnect with a leading `stream_gap` event (the
   client's cursor predates the history ring, so some events were lost),
   the watchdog **escalates every operation it is currently tracking** —
   it can no longer be sure those operations completed — and then resumes
   from the live tail.
3. If every reconnect attempt fails, the watchdog escalates **"rp
   unresponsive"** — this is the end-to-end liveness trigger the design
   calls out: a hung or crashed rp is itself a watchdog event, not just a
   silent gap.

A clean reconnect that replays without a gap resumes tracking
transparently: completions that arrived during the brief disconnect clear
their operations on replay, and still-open operations keep their original
timers.

### Escalation (corrective-action ladder)

On expiry the watchdog runs the corrective action selected by the
operation family's `on_expiry` policy:

- **`notify_only`** — dispatch a `Notification` through the same `Notifier`
  chain the safety monitor uses (so a Pushover alert fires) and record it
  in the dashboard notification history. This is the default, and it is
  also the *only* action for the liveness triggers below (a `stream_gap`
  or an unresponsive rp has no single service to abort).
- **`abort_then_restart`** — run an **escalating ladder** against the
  service that owns the family (named by `operations.<family>.service` and
  resolved against the [discovered services](#service-discovery)), then
  notify. The ladder takes the least-invasive action that can clear the
  stall, in order:
  1. **Health check** — `GET {base_url}/{device}/0/connected` with a 2 s
     timeout, where `base_url` is derived from the service's `<svc>.json`
     (`{scheme}://{host}:{port}/api/v1` with the same host derivation as the
     health probe — see
     [Service discovery](#deriving-the-health-probe-url)) and the device
     number is always `0` (one service per device, ADR-014). A clean `200`
     means the service is alive and the *operation* is stuck; anything else
     (non-200, timeout, connection refused) means the service itself is
     unresponsive.
  2. **Abort** — if the service is responsive and the family maps to an
     ASCOM abort verb (`slew`/`park` → `telescope/{n}/abortslew`,
     `exposure` → `camera/{n}/abortexposure`, `move_focuser` →
     `focuser/{n}/halt`), `PUT` that verb. A successful abort **ends the
     ladder** — the aborted operation surfaces a `*_failed` / `*_complete`
     on the stream, which clears its tracking entry.
  3. **Restart** — if the service is unresponsive, the abort failed, or the
     family has no abort verb (e.g. the compound `centering`), run the
     derived restart command (bounded by the constant 300 s restart budget)
     and then poll the health check until the service is responsive again or
     the budget elapses. The rung first acquires the service's slot in
     the [shared restart gate](#the-shared-restart-gate); if another restart
     of the same service is already in flight (REST endpoint or health
     supervision), the rung is skipped and the escalation message reports
     `restart=skipped(already in flight)`.
  4. **Notify** — always, through the `Notifier` chain, with a message that
     reports which rungs ran and their outcome (rendered into the
     `%action%` placeholder).

A family configured `abort_then_restart` whose `service` cannot be
resolved (no `service` set, or a name that no
[discovered service](#service-discovery) carries) **degrades safely to
`notify_only`** with a logged warning — a config mistake never
aborts the wrong device or wedges the watchdog (tenet #2, robustness).
A discovered service whose `<svc>.json` cannot be read has no derivable
URL, so it cannot be health-checked or aborted (health reports *unknown*,
abort is skipped) and the ladder falls through to the restart rung.

> **End-to-end coverage.** The ladder's rungs are unit-tested
> (`services/sentinel/src/corrective.rs`) and the watchdog's policy branching +
> SSE/liveness handling are unit- and BDD-tested against stubs
> (`services/sentinel/tests/features/operation_watchdog.feature`). The full
> two-process loop — a **real rp** emitting over `/api/events/subscribe` and a
> **real sentinel** subscribing, escalating, and shelling out the restart
> command — runs in the dedicated harness
> [`tests/operation-watchdog-e2e/`](../../tests/operation-watchdog-e2e). It
> drives the watchdog via `center_on_target`: rp advertises a centering
> deadline but does **not** enforce it (advisory, §2.5), so the watchdog timer
> is the only thing that fires, and because `centering` has no Alpaca binding
> the ladder skips abort and exercises the **restart rung** end-to-end (the
> rung otherwise only unit-tested). Its three scenarios cover wedge → restart,
> a converging op that is *not* escalated, and rp going away → "unresponsive".

> **Deferred — rp-side recovery callbacks.** The plan
> (`docs/plans/archive/predictive-deadlines-and-watchdog.md` §5.3–5.4) sketches two
> follow-up POSTs from sentinel to rp — `/api/internal/operation-aborted`
> (clear sticky state, re-plan) and `/api/internal/service-restarted`
> (reconnect the driver). They are **not** implemented: rp has no
> abort-recovery or reconnect-after-restart machinery to consume them
> today, so the endpoints would be no-ops. They land with that rp-side
> recovery work, not here.

### Configuration

The watchdog is configured by an optional top-level `operation_watchdog`
block. The services it can health-check, abort, and restart are the
[discovered services](#service-discovery), referenced by name in
`operations.<family>.service`:

```json
{
  "operation_watchdog": {
    "rp_url": "http://localhost:8080",
    "reconnect_max_attempts": 5,
    "reconnect_backoff": "5s",
    "default_buffer": "10s",
    "notifiers": ["pushover"],
    "message_template": "Operation %operation% (%operation_id%) %reason% after %elapsed%%action%",
    "operations": {
      "slew":         { "buffer": "5s",  "on_expiry": "abort_then_restart", "service": "star-adventurer-gti" },
      "park":         { "buffer": "30s", "on_expiry": "notify_only"        },
      "exposure":     { "buffer": "30s", "on_expiry": "abort_then_restart", "service": "qhy-camera"          },
      "centering":    { "buffer": "0s",  "on_expiry": "notify_only"        },
      "move_focuser": { "buffer": "5s",  "on_expiry": "abort_then_restart", "service": "qhy-focuser"         }
    }
  }
}
```

| Field | Default | Meaning |
|---|---|---|
| `rp_url` | *(required)* | Base URL of the rp instance to watch; the watchdog subscribes to `{rp_url}/api/events/subscribe`. |
| `reconnect_max_attempts` | `5` | How many consecutive reconnect attempts before escalating "rp unresponsive". |
| `reconnect_backoff` | `5s` | Delay between reconnect attempts (humantime). |
| `default_buffer` | `10s` | Buffer added to `max_duration_ms` for families with no `operations` entry. |
| `notifiers` | *(all)* | Which notifier `type`s receive escalations; omitted means every configured notifier. |
| `message_template` | built-in | Escalation message; placeholders `%operation%`, `%operation_id%`, `%elapsed%` (rendered as a humantime string, e.g. `5m 5s`), `%reason%`, `%action%` (the corrective-action summary, empty for `notify_only`). |
| `operations.<family>.buffer` | `default_buffer` | Buffer for this operation family. |
| `operations.<family>.on_expiry` | `notify_only` | Corrective-action policy: `notify_only`, or `abort_then_restart` (runs the ladder against `service`). |
| `operations.<family>.service` | *(none)* | Name of the [discovered service](#service-discovery) that owns this family (`dsd-fp2`, not `rusty-photon-dsd-fp2`). Required for `abort_then_restart`; ignored otherwise. |

The restart rung's time budget is the constant 300 s restart budget shared
with every other restart path (see
[Service Health Supervision](#service-health-supervision)).

`reconnect_max_attempts` of `0` means "never give up reconnecting" (the
watchdog keeps retrying without ever escalating an unresponsive rp), and a
`reconnect_backoff` of `0s` retries immediately.

### Edge cases

| Scenario | Behavior |
|----------|----------|
| Operation completes within its deadline | Tracking entry removed on `*_complete` / `*_failed`. No notification. |
| Operation's `max_duration_ms + buffer` elapses with no completion | One escalation per expiry, recorded in history (and, for `abort_then_restart`, after the ladder runs). |
| Expiry, `abort_then_restart`, service responsive | Health check passes → abort verb `PUT`; ladder stops; notification reports `abort=ok`. |
| Expiry, `abort_then_restart`, service unresponsive | Abort skipped → derived restart command run → recovery awaited up to the 300 s budget; notification reports the restart outcome. |
| Expiry, `abort_then_restart`, family has no abort verb (`centering`, `plate_solve`) | Abort skipped; restart attempted. |
| Expiry, `abort_then_restart`, service's `<svc>.json` unreadable | Health reports unknown, abort skipped; restart attempted. |
| Expiry, `abort_then_restart`, `service` unset or not discovered | Degrades to `notify_only` with a logged warning. |
| `*_started` without `max_duration_ms` | Tracked open, no timer; clears on completion or on a liveness escalation. |
| Stream drops, reconnect succeeds within `reconnect_max_attempts` | Replays buffered events, resumes tracking. No escalation unless a `stream_gap` is reported. |
| Reconnect reports `stream_gap` | Every currently-tracked operation is escalated (completions may have been lost); tracking resumes from the live tail. |
| rp unreachable for `reconnect_max_attempts` reconnects | "rp unresponsive" escalation. |
| `operation_watchdog` block absent | Watchdog disabled; sentinel runs safety polling only. |

## Service Health Supervision

Health supervision is sentinel's third monitoring loop. Where the safety
monitor answers "is the sky safe?" and the watchdog answers "is the running
operation making progress?", health supervision answers **"is this service
process alive right now?"** — and restarts it autonomously when it is not.
It closes the gap the plate-solver plan deferred (see
[`plate-solver.md` §Supervision and recovery](plate-solver.md#supervision-and-recovery)):
a service that SIGSEGVs is relaunched by the OS supervisor, but a service
that *hangs* — process alive, HTTP dead — previously needed an external
`/health`-polling watchdog. This loop is that watchdog.

Supervision is **universal and unconfigured**: every
[discovered service](#service-discovery) in the `running` or `failed` run
state is supervised — there is no opt-in block, so forgetting one can never
silently mean no supervision. Each supervised service gets its own
`ServiceHealthSupervisor` task, spawned and reaped by the discovery loop as
services are installed, removed, stopped, and started. All supervision policy
is **constants** (the shipped defaults of the retired per-service config,
promoted):

| Constant | Value | Meaning |
|---|---|---|
| probe timeout | `2s` | Per-probe bound (the same bound as the watchdog ladder's health rung). The probed endpoint should therefore be cheap; `plate-solver`'s `/health` (two filesystem stats, no subprocess) is the reference shape. |
| poll interval | `30s` | Probe cadence. Probing continues at this cadence during outages and backoff — only *restarting* is throttled. |
| failure threshold | `3` | Consecutive failed probes before the first autonomous restart. |
| restart backoff | `60s` | Wait before a second restart attempt when the first did not cure the service, doubling after every attempt. |
| restart backoff max | `900s` | Ceiling for the doubling backoff. |
| restart budget | `300s` | Time budget for a restart command *and* its recovery wait together — shared by the REST endpoint, the watchdog ladder, and health supervision. |

The probe is a `GET` of the service's
[derived health URL](#deriving-the-health-probe-url), carrying the
[observatory credential](#the-observatory-probe-credential) as HTTP Basic
auth when `service_auth` is configured. Alive means a `200`, **or a
`401`/`403`** — a service that challenges the probe has proven it is up
(the target may hold a hand-set credential, and aliveness must not depend
on the pair matching; `doctor` diagnoses a mismatched pair as
`auth.mismatch`) — **or a `503`**, which is alive but **degraded**: a
service that deliberately answers `503` has a live HTTP loop and is
reporting that an external dependency is unavailable (phd2-guider without
a running PHD2, plate-solver without its ASTAP binary or star database) —
a condition a service restart cannot cure, so supervision must not try
(issue #595). A degraded answer resets the outage exactly like a healthy
one: no failure counting, no restart, no notification; the dashboard shows
the service amber instead of green. Any other status, a timeout, or a
connection error counts as a failed probe.

The response body is never *interpreted* (health bodies are not uniform
across services, and no supervision decision may ever depend on one), with
one strictly display-only exception: on a `503` — and only then —
sentinel makes a best-effort attempt to read an optional top-level
`message` string from a JSON body and passes it through **verbatim and
opaque** to the dashboard next to the amber badge, truncated to 200
characters and HTML-escaped at render (bodies over 4 KiB are not parsed
at all — the supervision loop does bounded work per probe). The message lets a service say
*why* it is degraded (`"no connection to PHD2 on localhost:4400"`) in the
operator's face without sentinel understanding or acting on a word of it.
A missing, non-JSON, or message-less body simply shows no message.

### Behavior

- **Detection.** After the failure threshold, the supervisor runs the
  service's derived restart command through the same
  [`RestartManager`](#service-restart-api) engine the REST endpoint uses —
  bounded by the restart budget, with the derived recovery check
  (`systemctl is-active`-style, where the platform has one), and holding the
  service's slot in the shared restart gate (below).
- **`failed` units.** A unit the OS supervisor gave up on (`Restart=on-failure`
  exhausted) has no HTTP to probe; it is restarted directly on the same
  threshold-and-backoff state machine. Sentinel never gives up where systemd
  does.
- **Backoff.** Each restart attempt schedules the *next* allowed attempt
  one backoff later, doubling up to the ceiling. Probing never stops and the
  supervisor **never gives up** — a service that stays broken is retried at
  the capped cadence until it recovers or an operator intervenes.
- **Recovery.** One successful probe ends the outage: the failure counter,
  the backoff, and the restart schedule all reset. Recovery is visible on the
  dashboard but is deliberately **not** notified.
- **Degraded.** A `503` answer counts as recovery for the state machine
  (all counters reset) but publishes health `degraded` — with the opaque
  pass-through `message` when the body carries one — instead of `up`.
  Degraded is a normal operating state (a parked rig's guider spends every
  daylight hour there), so it is never notified; the amber dashboard row is
  the signal.
- **Underivable probe.** A `running` service whose `<svc>.json` cannot be
  read has no probe URL: its health reports `unknown`, no probe-driven
  restart fires, and the derivation is retried every discovery cycle.

### Notifications

Autonomous restarts — unlike [manual REST restarts](#service-restart-api),
which are silent — always notify through the full `Notifier` chain and are
recorded in the dashboard notification history:

| Trigger | Priority | Message shape |
|---|---|---|
| First restart of an outage | `0` | Service unhealthy after N consecutive failed probes; restarted autonomously, with the restart outcome (`status` / `recovery`). |
| Every later restart of the same outage | `1` (escalated) | Service **still unhealthy** after K autonomous restarts, with the latest outcome and the next-attempt time. |

Notification volume is bounded by construction: at most one notification per
restart attempt, and attempts are backoff-throttled (at the `900s` cap,
at most one escalation per 15 minutes per service). There is no separate
dedup or cooldown mechanism.

### The shared restart gate

All three restart paths — the REST endpoint, the watchdog ladder's restart
rung, and health supervision — acquire the same per-service in-flight slot
(`RestartGate` in `restart.rs`) before running a restart, so at most
one restart of a given service runs at any time:

- REST endpoint blocked by the gate → `409` (already in flight), unchanged.
- Watchdog ladder blocked by the gate → skips its restart rung; the
  escalation message reports `restart=skipped(already in flight)`.
- Health supervision blocked by the gate → logs at debug and keeps probing;
  no notification, no counter or backoff change. Whatever restart is already
  running will show its effect in the next probes.

### Edge cases

| Scenario | Behavior |
|----------|----------|
| Sentinel starts before a supervised service finishes booting | First probes fail but nothing restarts until threshold × poll interval (90 s) has elapsed — natural startup grace. A restart of a still-booting service is harmless for `systemctl restart`-style commands. |
| Manual restart (REST/UI) during an outage | The autonomous attempt finds the gate held and skips silently; if the manual restart cures the service, the next successful probe resets the outage. |
| Restart command fails (non-zero, spawn failure, over budget) | Counts as an attempt: notification carries the failure detail, backoff advances, probing continues. |
| Service flaps (recovers, fails again) | Each recovery fully resets the state machine; a new outage starts at 3 fresh failures and the initial 60 s backoff. |
| Service answers `503` mid-outage | Same reset as a recovery — the HTTP loop answering proves whatever the restarts were for is over; the service is now waiting on a dependency no restart can supply. |
| Shutdown during an in-flight autonomous restart | The supervisor returns immediately (restart await is raced against the cancellation token); the gate slot is released and the shell child runs to completion detached. |
| Operator stops a service mid-outage (`running` → `stopped`) | Its supervisor is stood down on the next discovery refresh (≤ 60 s); no further probes or restarts. The threshold-and-90-s detection window means an operator stop is seen before any probe-driven restart can fire. |
| Package removed mid-outage | The service leaves the discovered set; its supervisor and dashboard entry are reaped. |
| Service is `inert`, `stopped`, or `disabled` | Displayed on the dashboard, never probed, never restarted, never notified. |

## Service Restart API

Sentinel owns **process restart** for the rest of the stack (`rp.md`
§Sentinel Watchdog Integration; the config-actions plan's service-lifecycle
split: drivers reload *themselves* via `config.apply`, sentinel restarts
*processes*). The dashboard router exposes one endpoint:

```
POST /api/services/{name}/restart
```

`{name}` is the name of a [discovered service](#service-discovery)
(`dsd-fp2`, not `rusty-photon-dsd-fp2`). Sentinel does **not** spawn
or own the processes — it shells out to the derived restart command (the OS
supervisor owns relaunch), then polls the derived recovery check
(`systemctl is-active`-style; on platforms without one, recovery
confirmation is skipped) until it exits 0 or the 300 s restart budget
elapses (each probe is bounded to its per-attempt slice of the budget, so
one hanging probe is killed and retried rather than consuming the whole
phase). The manual restart is the operator's recovery hammer, so it works
for **any** discovered service regardless of run state — including `stopped`
and `inert` ones autonomous supervision leaves alone. The endpoint is what
the web UI's *Restart via Sentinel* affordance calls (see
[`ui-htmx.md`](ui-htmx.md)), and the escalation target for `config.apply`
fields classified `restart_required`.

### Behavioral contract

| Condition | Response |
|---|---|
| Restart command exits 0; recovery check passes within the remaining budget | `200` `{"service":"<name>","status":"ok","recovery":"healthy"}` |
| Restart command exits 0; recovery check never passes in budget | `200` `{"service":"<name>","status":"ok","recovery":"timeout"}` |
| Restart command exits 0; platform has no recovery check (macOS) | `200` `{"service":"<name>","status":"ok","recovery":"skipped"}` |
| Restart command exits non-zero, fails to spawn, or exceeds the budget | `200` `{"service":"<name>","status":"failed","detail":"…"}` (no recovery poll) |
| `{name}` is not a discovered service | `404` `{"error":"no discovered service named '<name>'"}` |
| A restart for `{name}` is already in flight | `409` `{"error":"a restart of '<name>' is already in flight"}` |

- **`status` reports the action's outcome, not the transport's** — a failed
  restart command is a domain result (HTTP 200 with `status:"failed"`),
  mirroring the config-actions protocol's `status:"invalid"` convention.
  4xx is reserved for addressing errors (unknown / not restartable / busy).
- **One restart per service at a time.** Concurrent POSTs for the same
  service are rejected with `409` while the first is running; different
  services restart independently. The in-flight slot is the
  [shared restart gate](#the-shared-restart-gate), so a `409` can also mean an
  autonomous restart (health supervision) or a watchdog-ladder restart is
  currently running for that service.
- **The request blocks** until the command (and recovery poll, if any)
  completes — bounded by the 300 s restart budget, so the
  response always arrives within that budget plus scheduling noise.
- **Same protection as the rest of the dashboard.** The endpoint sits on the
  dashboard router, behind the same optional `server.auth` Basic-auth and
  `server.tls` layers — there is no separate gate. Deploying without dashboard auth
  leaves it (like the JSON API) open; the restart command is derived from the
  discovered unit name, never taken from the request.
- A user-initiated restart is logged (`info!`) but does **not** dispatch
  notifications and is not recorded in the notification history — the
  operator who pressed the button already knows.

- **Web UI**: Server-rendered HTML with JavaScript polling (auto-refreshes every 5 seconds). Shows monitor statuses, a *Discovered Services* table (run state + health), and the notification history.
- **JSON API**: `/api/status` (monitor statuses), `/api/services` (discovered-service state and health, below), `/api/history` (notification history)
- **Service restart**: `POST /api/services/{name}/restart` (see [Service Restart API](#service-restart-api))
- **Health check**: `/health` returns 200 OK

### `GET /api/services`

Returns one entry per [discovered service](#service-discovery), sorted by
name — an empty array when nothing is discovered:

```json
[{
  "name": "plate-solver",
  "unit": "rusty-photon-plate-solver",
  "run_state": "running",
  "health": "up",
  "health_message": null,
  "last_probe_epoch_ms": 1760000000000,
  "consecutive_failures": 0,
  "restarts_in_outage": 0,
  "total_restarts": 3,
  "next_restart_epoch_ms": null,
  "probe_port": 11131,
  "poll_interval_ms": 30000
}]
```

- `run_state` is the discovery classification: `"running"`, `"failed"`,
  `"inert"`, `"stopped"`, or `"disabled"`.
- `health` is `"unknown"` (never probed, no derivable probe URL, or not in a
  probed run state), `"up"`, `"degraded"` (the probe answered `503`:
  alive, but an external dependency is unavailable — see
  [Service Health Supervision](#service-health-supervision)), or `"down"`.
- `health_message` is the opaque `message` string from a degraded probe's
  `503` JSON body (truncated to 200 characters), or `null` — always `null`
  when `health` is not `"degraded"`. Sentinel passes it through for display
  and never interprets it.
- `last_probe_epoch_ms` is `0` until the first probe completes.
- `restarts_in_outage` counts autonomous restarts in the current outage
  (resets on recovery); `total_restarts` counts them since sentinel started.
- `next_restart_epoch_ms` is the backoff-scheduled earliest next autonomous
  restart, or `null` when none is scheduled.
- `probe_port` is the service's listening port from its config's `server`
  block (what the derived probe URLs embed), or `null` when no probe is
  derivable. Clients use it to match a device URL to the service behind it:
  ui-htmx resolves a roster device's `alpaca_url` port against this list to
  offer that device's Restart-via-Sentinel affordance.

The dashboard is server-rendered HTML with vanilla JavaScript, built with `format!()` in `services/sentinel/src/dashboard.rs`. (An experimental `sentinel-app` Leptos/WASM frontend was scaffolded and later abandoned; it was removed in 2026-06 — see [docs/plans/archive/sentinel-app-leptos-dashboard.md](../plans/archive/sentinel-app-leptos-dashboard.md).)

## Module Structure

```
services/sentinel/src/
  main.rs              CLI entry point
  lib.rs               Module declarations + SentinelBuilder + Sentinel + Config factory methods
  config.rs            Config types + load_config()
  error.rs             SentinelError + Result alias
  io.rs                HttpClient trait + ReqwestHttpClient
  monitor.rs           Monitor trait + MonitorState + StateChange
  alpaca_client.rs     AlpacaSafetyMonitor (Monitor impl)
  watchdog.rs          EventMonitor trait + OperationDeadlineMonitor + SSE event source + deadline tracking
  corrective.rs        Corrective-action ladder: HealthChecker/Aborter/Restarter traits + HTTP/shell impls + CorrectiveLadder
  discovery.rs         ServiceManager trait (systemd/SCM/brew backends + test stub) + unit enumeration, run-state classification, probe-URL derivation
  restart.rs           RestartManager: discovered-service registry + shared RestartGate + restart/recovery orchestration
  health.rs            ServiceHealthSupervisor: periodic health probes + autonomous restart with backoff; discovery loop spawns/reaps one per supervised service
  notifier.rs          Notifier trait + Notification types
  pushover.rs          PushoverNotifier (Notifier impl)
  engine.rs            Engine: polling loops + transition matching + dispatch
  state.rs             SharedState: monitor statuses + notification history
  dashboard.rs         axum Router: HTML dashboard + JSON API
```

## Edge Cases

| Scenario | Behavior |
|----------|----------|
| Initial state (first poll) | Unknown to Safe/Unsafe. No notification by default. |
| Device unreachable | Returns MonitorState::Unknown. No notification. Increments error counter. |
| Device recovers | Unknown to Safe/Unsafe can trigger notification if configured. Error counter resets. |
| Pushover failure | Log warn, record failure in history. No retry. |
| ASCOM error response | Treated as Unknown (same as unreachable). |
| Rapid flapping | Every real transition triggers notification. |
| Empty transitions | Valid config. Monitors run, dashboard works, no notifications. |
| Dashboard port conflict | Log error, continue without dashboard. |

## Running

```bash
# With config file
cargo run -p sentinel -- -c services/sentinel/examples/config.json

# With CLI overrides
cargo run -p sentinel -- -c config.json --dashboard-port 8080

# Debug logging
cargo run -p sentinel -- -c config.json -l debug
```

Without `-c`, the config path resolves to the platform config
directory (`~/.config/rusty-photon/sentinel.json` on Linux,
`%PROGRAMDATA%\rusty-photon\sentinel.json` on Windows) and a default
config file is created there on first start if none exists — the same
convention as the driver services (see
[ADR-012](../decisions/012-service-packaging-architecture.md)).

`sentinel doctor [--config <file>] [--json]` diagnoses this service's own
config read-only without starting it — see
[doctor.md §Per-service doctors](doctor.md). Top-level flags cannot be
combined with the subcommand (the mixed form would silently ignore them).

Sentinel's package is also the delivery vehicle for central doctor
(ADR-016 decision: no separate `rusty-photon-doctor` package): the
deb/rpm, the MSI Core feature, and the Homebrew formula each carry the
`rusty-photon-doctor` binary plus the platform's daily TLS-renewal
scheduling (`rusty-photon-renew.service`/`.timer` on Linux, a Scheduled
Task on Windows, a launchd plist in the keg on macOS — see
[doctor.md §Renewal](doctor.md) and the per-platform packaging guides).

## Port

11114 (dashboard, configurable)

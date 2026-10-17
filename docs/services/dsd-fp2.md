# Deep Sky Dad FP2 Service

## Overview

The `dsd-fp2` service is an ASCOM Alpaca **CoverCalibrator** driver for the
Deep Sky Dad **Flat Panel 2 (FP2)** — a motorised flat-field panel that
combines an electroluminescent light source with a servo-driven cover. The
device speaks a bracketed ASCII protocol over a USB-CDC serial port
(`/dev/ttyACM*` on Linux, vendor/product `2e8a:000a`).

The driver exposes the FP2 as a single ASCOM Alpaca CoverCalibrator device so
that the existing `calibrator-flats` tool provider (which already consumes
`open_cover` / `close_cover` / `calibrator_on` / `calibrator_off` via `rp`'s
MCP tools) can drive it without any changes on its side.

## Architecture

The service is built on the workspace's `rusty-photon-shared-transport`
crate (PR #269), which factors out the refcounted-lifecycle scaffolding
that previously lived per-service. The crate provides:
`SharedTransport<C>` (the lifecycle/refcount core), `Codec` (typed
command/response translation), `FrameTransport` (one open conduit),
`TransportFactory` (open-me-a-transport), `Session<C>` (per-device
handle), and `Hooks` (per-service handshake / teardown / while-open
plug).

```
+----------------------------------------------------------------+
|  dsd-fp2 binary                                                |
|                                                                |
|  main.rs ──► lib.rs (ServerBuilder, BoundServer)               |
|                  │                                             |
|                  ▼                                             |
|        DsdFp2Device (device.rs)                                |
|             │  session: Session<Fp2Codec>                      |
|             │  set_connected → transport.acquire() / close()   |
|             ▼                                                  |
|        FlatPanelManager (manager.rs)                           |
|             ├─ cached_state (motor/cover/light/brightness/…)   |
|             └─ transport: Arc<SharedTransport<Fp2Codec>>       |
|                  │  Hooks: handshake = [GFRM] verify + seed    |
|                  │         while_open = 500 ms poll loop       |
|                  │         teardown = noop                     |
|                  ▼                                             |
|        Fp2Codec (codec.rs)  ◀── encodes Command → bytes        |
|                                  decodes (VALUE) → RawResponse |
|                  │                                             |
|                  ▼                                             |
|        Fp2SerialTransportFactory (transport.rs)                |
|             ▶ open_serial_port → tokio_serial::SerialStream    |
|             ▶ SerialFrameTransport (read until b')')           |
|                                                                |
|   tests:    MockTransportFactory (mock.rs, feature "mock")     |
|             ▶ MockFrameTransport with stateful sim             |
|                                                                |
|        /dev/ttyACM0  @ 115200 8N1   (FP2 hardware)             |
+----------------------------------------------------------------+
```

Module responsibilities:

- **Protocol layer** (`protocol.rs`): `Command` enum, `RawResponse` body
  parsers (`parse_ok` / `parse_int` / `parse_bool` / `parse_temperature`
  / `parse_firmware`).
- **Codec** (`codec.rs`): `Fp2Codec` implements
  `rusty_photon_shared_transport::Codec` — `encode(Command) -> Vec<u8>`,
  `decode(&[u8]) -> RawResponse`. FP2 has no unsolicited frames, so
  default `matches` (true) and `max_skip` (0) are correct.
- **Transport factory** (`transport.rs`):
  `Fp2SerialTransportFactory` opens the port through the shared crate's
  `open_serial_port` (one opener for every serial driver: builder
  settings, error mapping, and the bounded retry that rides out a
  Windows handle still closing), asserts DTR on the stream it returns,
  and wraps it in `SerialFrameTransport` with `terminator = b')'` and a
  256-byte frame-size cap.
- **Manager** (`manager.rs`): `FlatPanelManager` owns
  `Arc<SharedTransport<Fp2Codec>>` and the `Arc<RwLock<CachedState>>`
  written by the while-open poll task and the handshake hook. No
  lifecycle plumbing of its own.
- **ASCOM device** (`device.rs`): `DsdFp2Device` holds the
  manager and a per-device `Session<Fp2Codec>`. `set_connected(true)`
  calls `manager.transport().acquire().await`; `set_connected(false)`
  awaits `session.close().await` (primary teardown path,
  per the shared-transport contract). Cover and calibrator state derive
  from the cached snapshot.
- **Server builder** (`lib.rs`): Wires the device into an
  `ascom_alpaca::Server` with TLS and auth (via `rusty-photon-tls`, `rp-auth`)
  and returns a `BoundServer`.
- **Mock mode** (`mock.rs`, feature `mock`):
  `MockTransportFactory` returns in-process `MockFrameTransport`s
  driven by a shared simulator state. Used by BDD scenarios, ConformU,
  and unit tests.

What is **not** in this crate (because the shared-transport crate owns it):

- Refcounting of clients (`SharedTransport.count`).
- Request arbitration (`Connection.transport` Mutex).
- Spawning and cancelling the while-open task.
- Rollback on handshake failure (Phase A's `RollbackGuard`).
- `Session::close()`'s synchronous teardown contract and the
  `Session::drop` detached-cleanup fallback.

## Hardware Constraints

- **Connection**: USB-CDC virtual serial port, vendor/product `2e8a:000a`,
  product descriptor `Deep Sky Dad FP2`. Stable path is
  `/dev/serial/by-id/usb-Deep_Sky_Dad_Deep_Sky_Dad_FP2_<serial>-if00`.
  `pkg/doctor.toml` declares `usb_model = "FP2"`, matched as a substring of
  that descriptor — `2e8a:000a` is the RP2040's own id and identifies the
  microcontroller, not the panel.
- **DTR gates the firmware's output**: the RP2040's USB-CDC stack sends
  nothing until the host asserts DTR. Linux raises DTR when a tty is
  opened, so the requirement is invisible there; Windows does not, and a
  port opened without it accepts every command and answers none — the
  `[GFRM]` handshake times out and the service crash-loops. The transport
  factory therefore asserts DTR on the opened stream on every platform.
  It cannot rely on the serial builder's `dtr_on_open` flag: on Windows
  the async open probes the port through a synchronous handle (where that
  flag is applied), closes it — dropping DTR — and reopens the path in
  overlapped mode, re-applying only the line settings, so the flag never
  reaches the handle that is kept. Verified on a Windows 11 host against
  firmware 1.0.4: silent without DTR, answering on the same handle once
  DTR was raised; a build with only `dtr_on_open(true)` still timed out.
- **Cover travel**: a servo sweeping nominally 0° (open) to 270° (closed).
  Targets outside the bounds are rejected by the firmware.
- **Brightness range**: 12-bit, `0..=4096` (inclusive at both ends per the
  INDI reference driver).
- **Brightness linearity**: the EL panel's optical output is not linear
  across the full `0..=4096` range. A central-quarter-frame photometric
  sweep against a cooled QHY178M (gain 0, non-saturating exposure, 5
  repeats/level, 2026-07-12) found a hard dead zone at raw brightness 1-2
  (no measurable light above the sensor's bias level), a steep sub-linear
  ramp through roughly 3-100, and a crossover into the ASCOM spec's ±5%
  linearity band at approximately raw brightness 110, with a small
  residual bias lingering until ~250. From 250-4096 the response is
  linear (R² > 0.9999, residuals ≤ 1.8%). `cover_calibrator.min_brightness`
  (default `250`, see [Configuration](#configuration)) rejects non-zero
  `calibrator_on` requests below this floor rather than silently
  delivering an unreliable flat; `0` (the ASCOM "on at zero" state — see
  [Validation](#validation)) is always accepted regardless of the floor.
  The measurement was taken on one physical panel — EL turn-on thresholds
  can vary unit to unit, which is why the floor is a config field rather
  than a hardcoded constant.
- **No halt-cover support**: the FP2 protocol does not expose a
  cover-motion abort opcode; once started, a move runs to completion.
  The driver therefore returns `MethodNotImplementedException`
  (Alpaca error code 0x400) from `HaltCover` — which is what the
  [ASCOM ICoverCalibratorV2 spec][cc-spec] explicitly mandates "if cover
  movement cannot be interrupted".

  [cc-spec]: https://ascom-standards.org/newdocs/covercalibrator.html
- **Heater**: optional dew-heater channel with a thermistor. Present if
  `[GHTT]` returns a value greater than `-40` °C. The driver reads this
  for diagnostics but does not expose it as an ASCOM Switch in the MVP
  (see [Future Work](#future-work)).
- **Control modes**: the device has a local-button mode that auto-returns
  to "remote control" on the first received serial command or after a 10-minute
  idle. No driver state is needed to handle this — the panel restores
  remote mode the moment we ping it on connect.

## Protocol Reference

The FP2 uses **bracketed ASCII** at **115200 8N1**.

- **Command framing**: `[CMD...]` — opening `[`, ASCII payload, closing `]`.
  No terminator is added.
- **Response framing**: `(VALUE)` — opening `(`, ASCII payload, closing `)`.
  Read until `)`. Timeout: 3 s per request.
- **Frame ordering**: every `Session::request` and every poll-task
  request goes through the per-`Connection<Fp2Codec>` request lock in
  `rusty-photon-shared-transport` (the `Connection.transport` Mutex).
  Encode → `send_frame` → `recv_frame` → decode runs end-to-end while the
  lock is held, so foreground writes and the while-open poll cannot
  interleave bytes on the wire. The driver does not pre-drain the read
  buffer — `SerialFrameTransport` consumes one terminator-delimited
  frame per `recv_frame` call, and any leading bytes before the next
  `(` are tolerated by `RawResponse::from_frame`.

### Command Set

| Command       | Direction | Purpose                                      | Response                                  |
|---------------|-----------|----------------------------------------------|-------------------------------------------|
| `[GFRM]`      | Get       | Firmware identification                      | `(Board=DeepSkyDad.FP2, Version=X.Y.Z.W)` |
| `[GOPS]`      | Get       | FP2 cover state (binary)                     | `(0)` closed, `(1)` open, other in-between |
| `[GPOS]`      | Get       | Cover angle (FP1-style; also valid on FP2)   | `(0)` open … `(270)` closed                |
| `[GMOV]`      | Get       | Motor state                                  | `(0)` stopped, `(1)` running              |
| `[STRG<deg>]` | Set       | Set target angle (0..270)                    | `(OK)`                                    |
| `[SMOV]`      | Action    | Execute move to current target               | `(OK)`                                    |
| `[GLON]`      | Get       | Light enable state                           | `(0)` off, `(1)` on                       |
| `[SLON<0\|1>]`| Set       | Light on/off                                 | `(OK)`                                    |
| `[GLBR]`      | Get       | Brightness                                   | `(N)`  with `N` in `0..=4096`             |
| `[SLBR<NNNN>]`| Set       | Brightness (zero-padded 4 digits)            | `(OK)`                                    |
| `[GHTT]`      | Get       | Heater temperature (°C, float)               | `( 26.104242)` — leading-space tolerated  |
| `[GHTM]`      | Get       | Heater mode                                  | `(0)` off, `(1)` on, `(2)` on-when-active |
| `[SHTM<N>]`   | Set       | Heater mode (0\|1\|2)                        | `(OK)`                                    |

Commands not in the above table are unused by this driver. The FP1
variant of the protocol uses some additional commands (e.g. `[GPOS]`-only
state) that are exercised here only as a fallback / liveness ping.

### Ping / Liveness

The handshake on connect issues `[GFRM]`. A non-empty response identifying
"DeepSkyDad.FP2" succeeds; anything else fails the connection with a
clear error. The board / version strings are stashed in `CachedState`
(`firmware_board` / `firmware_version`) for diagnostics and future
heater-Switch use; ASCOM `driver_info()` returns a static driver
identity string and does not include them, per the convention that
`driver_info` describes the *driver*, not the connected hardware.

## ASCOM CoverCalibrator Mapping

ASCOM's `CoverCalibrator` interface unifies a motorised cover and a
calibration light source. The mapping is:

### CoverState Derivation

`CoverState` is derived from cached `[GMOV]` + `[GOPS]`:

| `[GMOV]` | `[GOPS]` | `CoverState`         |
|----------|----------|-----------------------|
| `1`      | _any_    | `Moving`              |
| `0`      | `0`      | `Closed`              |
| `0`      | `1`      | `Open`                |
| `0`      | other    | `Unknown`             |
| —        | —        | `Unknown` (disconnected) |

Codec-layer parse failures don't surface as `CoverState::Error`; they
propagate as `DsdFp2Error::MalformedResponse` through the request's
`Result` path, and the cached value the derivation reads from is simply
not refreshed for that tick.

### CalibratorState Derivation

`CalibratorState` is derived from cached `[GLON]`:

| `[GLON]` | `CalibratorState` |
|----------|-------------------|
| `0`      | `Off`             |
| `1`      | `Ready`           |
| —        | `Unknown` (disconnected) |

Same parse-failure note as above: a malformed `[GLON]` response
propagates as `DsdFp2Error::MalformedResponse`, not as
`CalibratorState::Error`.

The FP2's EL panel reaches commanded brightness in well under a poll
interval, so we never report `NotReady`. The default
`calibrator_changing()` impl (returns `state == NotReady`) yields `false`
without any driver-specific code.

### Method Mapping

| ASCOM method                  | FP2 wire sequence                       | Notes                                                                                               |
|-------------------------------|------------------------------------------|-----------------------------------------------------------------------------------------------------|
| `cover_state()`               | _(cached)_                              | Derived from cached `[GMOV]` + `[GOPS]`                                                              |
| `calibrator_state()`          | _(cached)_                              | Derived from cached `[GLON]`                                                                         |
| `brightness()`                | _(cached)_                              | Cached `[GLBR]`                                                                                      |
| `max_brightness()`            | _(cached)_                              | `min(config.max_brightness, MAX_BRIGHTNESS = 4096)` — the effective cap, advertised to ASCOM clients. |
| `open_cover()`                | `[STRG0]` → `(OK)` ; `[SMOV]` → `(OK)`  | Asynchronous; `cover_state` reports `Moving` until polled `[GMOV]→(0)`.                              |
| `close_cover()`               | `[STRG270]` → `(OK)` ; `[SMOV]` → `(OK)` | Same as above.                                                                                      |
| `halt_cover()`                | — (returns `MethodNotImplementedException`) | The FP2 firmware has no halt-motion opcode; per the ASCOM spec, `HaltCover` MUST throw `MethodNotImplementedException` when cover movement cannot be interrupted. |
| `calibrator_on(brightness)`   | `[SLBR<NNNN>]` → `(OK)` ; `[SLON1]` → `(OK)` | Reject `brightness` outside the effective `0..=max_brightness` range with `ASCOMError::INVALID_VALUE` (no clamping); pad the accepted value to 4 digits. Reject non-zero `brightness < config.min_brightness` (see [Brightness linearity](#hardware-constraints)). Sending brightness even when already on keeps the call idempotent. |
| `calibrator_off()`            | `[SLON0]` → `(OK)`                      | Does not change brightness; subsequent `calibrator_on(brightness)` reuses the prior commanded value if any. |
| `interface_version()`         | — (default)                             | Returns `2` (ICoverCalibratorV2).                                                                    |
| `cover_moving()`              | — (default)                             | Returns `cover_state == Moving`.                                                                     |
| `calibrator_changing()`       | — (default)                             | Returns `false` (we never report `NotReady`).                                                        |

### Validation

- `calibrator_on(brightness)`: validated against the effective max
  (`min(config.max_brightness, MAX_BRIGHTNESS = 4096)`) so the value
  `max_brightness()` advertises and the value `calibrator_on` accepts
  agree even when the config caps below the hardware ceiling. Brightness
  above the effective cap is rejected with `ASCOMError::INVALID_VALUE`.
  Zero is accepted and forwarded unchanged (the device treats
  `[SLBR0000]` + `[SLON1]` as "on at zero", which is what the spec calls
  for).
- `calibrator_on(brightness)`: also validated against
  `config.min_brightness` (default `250`) — a non-zero brightness below
  this floor is rejected with `ASCOMError::INVALID_VALUE` rather than
  silently driving the panel into its measured non-linear range (see
  [Brightness linearity](#hardware-constraints)). `0` is always accepted
  regardless of the floor, since it is the ASCOM "on at zero" state, not
  a dim request. A save on the setup pages rejects `min_brightness > max_brightness`
  (see [Validation rules](#validation-rules)), but a hand-edited config
  file loaded at startup isn't validated, so this inconsistent state is
  still reachable; when it is, the rejection message names it as a driver
  misconfiguration instead of suggesting an unreachable "raise the
  brightness" remediation.
- All writes (open/close, calibrator on/off, brightness changes) require
  `connected == true`; otherwise the driver returns `ASCOMError::NOT_CONNECTED`.
  `calibrator_on`'s brightness checks (max and min, above) run *before* the
  connection check, so an out-of-range or under-floor brightness returns
  `ASCOMError::INVALID_VALUE` even while disconnected — the value is invalid
  on its own terms regardless of hardware state.

## Connection Lifecycle

The lifecycle is the standard shared-transport flow; this section is
calibration for what specifically happens at each phase, not a
re-description of the shared-transport contract.

1. **`Connected = true`** on the ASCOM device calls
   `manager.transport().acquire().await`, which through
   `SharedTransport` increments the refcount and hands back a session.
   The port is already open and handshaken by then: `ServerBuilder::build()`
   calls `start()` before binding, and that is what calls
   `TransportFactory::open()` (serial at 115200 8N1), constructs the
   `Connection<Fp2Codec>`, runs the handshake hook, publishes the slot
   and spawns the while-open task.
2. The handshake hook sends `[GFRM]`, verifies the board is `DeepSkyDad.FP2`,
   and seeds the cached state with a single poll round. Failure (open
   error, non-FP2 firmware, malformed response, IO timeout) fails
   `start()`, and therefore `ServerBuilder::build()`: the service does
   not come up, rather than binding with a transport no client could
   use. Nothing is published, so there is no half-connected state to
   escape. On a later reconnect the same failure is reported as a
   failed attempt and retried at the configured cadence.
3. The while-open task ticks every `polling_interval` (default 500 ms),
   refreshing `[GMOV]`, `[GOPS]`, `[GLON]`, `[GLBR]`, `[GHTT]` into the
   cached state. The task uses `tokio::select!` against
   `ctx.cancelled()` so cancellation at teardown returns within one
   tick.
4. **`Connected = false`** calls `session.close().await`, which on the
   1→0 transition cancels the while-open task, awaits it (5 s timeout
   then abort), runs the teardown hook, and drops the slot's
   `Arc<Connection<C>>` (which closes the OS-level serial port).

The CoverCalibrator is the only device this server registers, so the
refcount tops out at `1` in normal use. The pattern is preserved so the
service can later expose a Switch device (heater control) without redesign.

## Configuration

```jsonc
{
  "serial": {
    "port": "/dev/ttyACM0",
    "baud_rate": 115200,
    "polling_interval": "500ms",
    "timeout": "3s"
  },
  "server": {
    "port": 11119,
    "bind_address": "0.0.0.0"
  },
  "cover_calibrator": {
    "name": "Deep Sky Dad FP2",
    "unique_id": "0c8d6f1a-3b2e-4a7c-9f1d-2e5b8c4a6d3f",
    "description": "Deep Sky Dad Flat Panel 2 (motorised flat field panel)",
    "enabled": true,
    "max_brightness": 4096,
    "min_brightness": 250
  }
}
```

The `server` block is the shared `AlpacaServerConfig` from
`crates/rusty-photon-server-config` (see ADR-016): `port`, `bind_address`
(default `0.0.0.0`), optional `discovery_port`, and optional `tls`/`auth`.
Absent `tls`/`auth` means plain, unauthenticated HTTP.

`min_brightness` (default `250`) is the floor below which `calibrator_on`
rejects a non-zero brightness — see [Brightness
linearity](#hardware-constraints) and [Validation](#validation). It was
measured on one physical panel; a different unit's EL turn-on threshold
may differ, so re-measure and adjust this value if flat quality looks off
near the floor on a different panel.

`serial.port` accepts either `/dev/ttyACM0`-style paths or the more
durable `/dev/serial/by-id/...-if00` form. Prefer the `by-id` form in
production configs so that re-enumeration of other USB devices does not
shuffle the FP2 onto a different `ttyACM*`. On Windows the field takes a
COM port name instead. The built-in default is platform-dependent —
`/dev/ttyACM0` on Unix, `COM3` on Windows — and is a placeholder either
way: the operator edits it to the real port.

Every block (`Config` and each nested config struct) rejects unknown keys at
deserialize (`deny_unknown_fields`), so a typo or a key removed by a schema
change fails loudly at load instead of being silently ignored.

### Device identity (UniqueID)

ASCOM requires each device's `UniqueID` to be **globally unique** and to
**never change**, but the Alpaca protocol enforces neither — uniqueness has
to come from how the id is generated. The driver no longer ships a hardcoded
literal (`"dsd-fp2-001"`). Instead, on **first run** it mints a spec-compliant
**UUIDv4** for `cover_calibrator.unique_id` and persists it to the resolved
config path (see [Config-path resolution](#config-path-resolution)).

The minting is done once at startup — before the config is read into the
running server — by `rusty_photon_config::resolve_and_init` (the shared
`rusty-photon-config` crate's canonical bootstrap, which also materializes the
default config file on first start; see [Config-path
resolution](#config-path-resolution)). Minting is **idempotent**: it fills the
id only when `cover_calibrator.unique_id` is absent or empty in the on-disk
file, persists atomically only when it actually filled something, and **never
overwrites** an id that already exists. It operates on the config *file*, not
the CLI-override-applied effective config, so a transient `--port` is never
baked in. The default `Config` therefore carries an **empty** `unique_id`; the
value above is illustrative of a minted id.

If the file leaves out the `cover_calibrator` section entirely, the bootstrap
copies it in from `Config::default()` before minting, so the section arrives
with its `name` and `description`. The bootstrap writes the file only when the
result loads. A file the driver would refuse (an unknown key, a section missing
a required field) is left byte for byte as it was, and the start fails with
`config file <path> is valid JSON but not a valid configuration: <detail>`. See
[docs/crates/rusty-photon-config.md](../crates/rusty-photon-config.md).

The only escape hatch for changing the id is the Identity disclosure on the
device's setup page. To prevent an operator from accidentally blanking the
device's stable identity, a save **rejects an empty (or
whitespace-only) `cover_calibrator.unique_id`** with a field error on
`cover_calibrator.unique_id` (see [Validation
rules](#validation-rules)). Editing the config file by hand to remove the id
is recoverable: the next startup re-mints a fresh one. Removing the whole
`cover_calibrator` section is recoverable the same way, with the section
re-filled from the defaults.

### CLI Arguments

| Argument | Description |
|----------|-------------|
| `-c, --config`     | Path to configuration file. If omitted, the driver resolves the platform default (e.g. `~/.config/rusty-photon/dsd-fp2.json` on Linux, `%PROGRAMDATA%\rusty-photon\dsd-fp2.json` on Windows), created on first start if absent. |
| `--port`           | Serial port path (overrides `serial.port`; pins it as a CLI override) |
| `--server-port`    | Server port (overrides `server.port`; pins it as a CLI override) |
| `-l, --log-level`  | Log level: trace, debug, info, warn, error |
| `--service`        | Hidden: run as a Windows service (passed by the Windows service control manager; no-op on other platforms) |

`dsd-fp2 doctor [--config <file>] [--json]` diagnoses this service's own
config read-only without starting it — see
[doctor.md §Per-service doctors](doctor.md). Top-level flags cannot be
combined with the subcommand (the mixed form would silently ignore them).

## Setup Pages

The driver configures itself through the two HTML pages the Alpaca
Management API defines, served by the driver itself behind its own TLS and
Basic Auth, with no other process in between
([driver-setup-pages.md](../plans/driver-setup-pages.md)):

| Path | Holds |
|------|-------|
| `/setup` | The server page: the `serial` section, the `server` section (port, bind address, discovery port; `tls` is file-only), the **Security** section (username, password, repeat → `server.auth`), and the device list with a link to the cover calibrator's page. |
| `/setup/v1/covercalibrator/0/setup` | The device page: the `cover_calibrator` section alone — name, description, `enabled`, `max_brightness`, `min_brightness`, and `unique_id` under a collapsed Identity disclosure. |

Both answer `GET` with the form and `POST` with the save. The pages are
rendered and saved by the shared
[`rusty-photon-setup`](../crates/rusty-photon-setup.md) crate; this
driver supplies only its config type, its validation and normalization,
which section its device edits (`device_sections`), its one secret
(`server.auth.password_hash`), and which fields its command-line flags
pin. The pages work whether or not the device is `Connected` — a wrong
`serial.port` must be fixable without first connecting to the thing the
bad port blocks connecting to.

The `config.get` / `config.apply` / `config.schema` actions this driver
used to expose are gone: `SupportedActions` is empty and every `Action`
answers `ACTION_NOT_IMPLEMENTED`.

### Config-path resolution

A persist target is **always** resolvable, in priority order:

1. `--config <path>` if given on the CLI, else
2. the **platform default** (via `rusty_photon_config::resolve_config_path`) —
   the per-user config directory on Unix:
   `~/.config/rusty-photon/dsd-fp2.json` on Linux (XDG;
   `$XDG_CONFIG_HOME/rusty-photon/...` when set),
   `~/Library/Application Support/rusty-photon/...` on macOS — and the
   machine-wide `%PROGRAMDATA%\rusty-photon\dsd-fp2.json` on Windows
   (`ProgramData` env var, `C:\ProgramData` fallback; a Windows service
   account's per-user profile is hidden, see ADR-015).

Startup bootstraps this path via `rusty_photon_config::resolve_and_init`: on
first start the **platform default** path receives the serialized
`Config::default()` (with the freshly-minted `unique_id`), so the file always
exists from then on and same-host consumers that read it — sentinel's
health-probe derivation, doctor — see it. A missing file at an **explicit**
`--config` path is still only created by the identity minting, never by the
default-materialization step. Startup and reload read the resolved path,
falling back to `Config::default()` if the file is somehow absent; a save on
a setup page **writes** it, creating parent directories as needed.

### What a page shows

- **The configuration file**, read through the typed `Config`: the base
  every save starts from. After a save's reload the running driver and
  the file agree; a hand edit of the file shows in the fields before a
  reload applies it, while the server page's device list (name,
  enabled) follows the running driver until then. A file the page
  cannot read is logged and the running configuration shown instead.
- A field pinned by a CLI override (`--port` → `serial.port`,
  `--server-port` → `server.port`) shows the running value, disabled,
  with a note saying so; a save never persists it.
- **No secret reaches a page**: the Security section carries no hash, and
  `server.tls` is named as file-only.

### Saving

A `POST` to either page submits that page's fields. The shared handler:

1. **Reads the file** as the base and overlays the submitted fields onto
   the page's section alone: a save of the device page cannot alter the
   `serial` or `server` section, and a save of the server page cannot alter
   `cover_calibrator`. Pinned fields keep the file's value. If the existing
   config file is *present but not valid JSON*, the save is refused rather
   than treating it as default and overwriting (losing) its contents.
2. **Validates** (table below) after `normalize` (which trims
   `serial.port`). On failure the form is answered again under `422` with
   each error beside its field and the submitted values kept, and the file
   is untouched.
3. **Persists** atomically (stage to a unique `NamedTempFile` in the target dir →
   fsync → rename → fsync dir; the random `O_EXCL` temp name avoids collisions
   between concurrent saves and symlink attacks).
4. **Answers** with a page that says the configuration was saved and that
   the driver is reloading, then **fires the in-process reload** *after the
   response is flushed* (see In-process reload). The page polls its own
   URL until the rebuilt server answers and then navigates to it; without
   script, a refresh header and a link do the same by hand. A save that
   changed nothing says so and fires no reload. When `server.port` or the
   bind address changed, the page names the URL the pages now live at.
5. A `POST` that does not come from the page itself (no `Sec-Fetch-Site`
   of `same-origin`/`none`, no `Origin` matching the request's `Host`) is
   refused with `403`.

#### Validation rules

| Field | Rule |
|-------|------|
| `serial.port` | non-empty |
| `serial.baud_rate` | `> 0` |
| `serial.polling_interval` | `> 0` |
| `serial.timeout` | `> 0` |
| `cover_calibrator.max_brightness` | `<= 4096` (hardware ceiling, `MAX_BRIGHTNESS`) |
| `cover_calibrator.min_brightness` | `<= cover_calibrator.max_brightness` |
| `cover_calibrator.unique_id` | non-empty (the device's stable ASCOM `UniqueID`; see [Device identity](#device-identity-uniqueid)) |
| `server.port` | any `u16` (`0` = OS-assigned, used in tests) |
| `server.auth` | a username needs a password, submitted or already stored; the repeat must match |

### In-process reload

A save triggers an **in-process reload**, not a process restart.
`main.rs` runs under `ServiceRunner::run_with_reload`: each loop iteration loads
the effective config and builds the server; a `ReloadSignal` held by the
setup-page handler (fired via `notify()`) makes the loop tear the current
server down and rebuild from the new file.

- **Fire-after-response.** The reload is fired from a detached task that yields
  briefly so the saved page flushes first — the server being torn
  down is the very one serving the request. The page's own wait covers the
  short connection blip.
- **Await the server's own teardown.** The loop passes `start()` a stop future
  that resolves on *either* the service shutdown *or* a reload, and **awaits
  `start()` to completion** rather than dropping it. So `start()`'s teardown
  always runs — gracefully draining HTTP connections and calling
  `manager.transport().shutdown()` to stop the reconnect supervisor and release
  the serial port — before the loop rebuilds. (An earlier design dropped the
  server future on reload, which skipped that teardown and could leak the port +
  supervisor across reloads under the service-lifetime transport model.)
- **Release the port, don't wait for a refcount.** `shutdown()` closes the
  transport explicitly rather than letting the last `Arc<Connection>` drop do
  it: a device that never called `Session::close` still holds a reference, and
  waiting for it would carry the open port into the rebuild. The shared crate
  does the same before every reconnect attempt, for the same reason.
- **The rebuild's open tolerates a handle still going away.** On Windows a COM
  port is exclusive per handle and `CloseHandle` lands after the stream drops —
  the pending overlapped read's cancellation has to reach the reactor first — so
  a re-open issued in the same breath can find the port still taken. Every
  serial driver opens through
  `rusty_photon_shared_transport::open_serial_port`, which retries on a short
  bounded ladder for exactly that window. A port genuinely held by another
  process still fails, with the OS's own error, about half a second later.
- **Clean HTTP rebind.** The rebuilt server binds the same `server.port` while a
  client's keep-alive connections may still linger on it. The listener is created
  with `SO_REUSEADDR` (`rusty_photon_tls::server::bind_dual_stack`), so the rebind succeeds
  immediately instead of failing with `AddrInUse`. (`SO_REUSEADDR` does not permit
  two live listeners on the port, so it can't mask an "already running" error.)
  A `setup_pages.feature` scenario drives a full save → reload → rebind cycle
  over the wire (pinning the OS-assigned port so the same port is rebound),
  guarding this OS-sensitive path in CI; `rusty-photon-tls` adds a focused unit test that
  rebinds a port with a lingering connection.

## Error Handling

All driver errors flow through `DsdFp2Error` (defined in `error.rs`) and
are converted to `ASCOMError` for protocol boundaries:

| Internal error               | ASCOM error code     |
|------------------------------|----------------------|
| `NotConnected`               | `NOT_CONNECTED`      |
| `HandshakeFailed(_)`         | `NOT_CONNECTED`      |
| `InvalidValue(String)`       | `INVALID_VALUE`      |
| `Timeout(_)`                 | `INVALID_OPERATION`  |
| `Communication(_)`           | `INVALID_OPERATION`  |
| `MalformedResponse(_)`       | `INVALID_OPERATION`  |
| `Io(_)`                      | `INVALID_OPERATION`  |
| `SerialPort(_)`              | `INVALID_OPERATION`  |

`debug!` is used for the per-operation breadcrumbs; only the
once-per-lifecycle messages (server bound, port opened, port closed) use
`info!` (CLAUDE.md rule 9).

## Module Structure

| Module                | Description                                                                                  |
|-----------------------|----------------------------------------------------------------------------------------------|
| `config.rs`           | `Config`, `SerialConfig`, `ServerConfig`, `CoverCalibratorConfig` + defaults + JSON load; `CliOverrides`, XDG path resolution, `load_effective_config` |
| `setup.rs`            | The driver's `ConfigurableDriver` impl behind the setup pages: `normalize`, `validate`, the secret pointer, the override paths, and `device_sections` (the cover calibrator's section) |
| `error.rs`            | `DsdFp2Error` (generated by `rusty_photon_driver::driver_error!`: the common transport-driver variants + `to_ascom_error()` + `From<TransportError>` + `From<DsdFp2Error> for ASCOMError`, plus device-specific `MalformedResponse` / `HandshakeFailed`) + `From<SessionError<…>>` |
| `protocol.rs`         | `Command` enum + `RawResponse` body parsers                                                  |
| `codec.rs`            | `Fp2Codec` — `rusty_photon_shared_transport::Codec` impl                                    |
| `transport.rs`        | `Fp2SerialTransportFactory` — opens via the shared `open_serial_port`, asserts DTR, wraps in `SerialFrameTransport` |
| `manager.rs`          | `FlatPanelManager` over `SharedTransport<Fp2Codec>` + `CachedState` + Hooks (handshake / while_open / teardown) |
| `device.rs`           | `DsdFp2Device` implementing ASCOM `Device + CoverCalibrator`; holds a per-device `Session<Fp2Codec>`; its `setup_request` answers the device setup page through the shared handler |
| `mock.rs`             | `MockTransportFactory` + `MockFrameTransport` (feature `mock`); shared `MockState` simulator |
| `lib.rs`              | Public exports, `ServerBuilder` (wires the setup-page handler into the device and the server), `BoundServer` |
| `main.rs`             | CLI (clap) + tracing init; lifecycle owned by `rusty-photon-service-lifecycle::ServiceRunner` |

## MVP Scope

### In Scope

- Connect / disconnect over USB-CDC serial
- Cover open / close + state polling (`CoverState` ∈ {`Open`, `Closed`, `Moving`, `Unknown`, `Error`})
- Calibrator on / off + brightness set (`CalibratorState` ∈ {`Off`, `Ready`, `Unknown`, `Error`})
- Brightness read (cached) and `max_brightness = 4096`
- **Setup pages** (`/setup` and the device page) with platform-default config
  path, layer-aware persist, validation, no secret on any page, and in-process reload
- Mock factory for ConformU and BDD (consumed by both the spawned
  binary and the in-process `manager.rs` mock tests)
- Four BDD feature files: connection_lifecycle, cover_control, calibrator_control,
  setup_pages
- Unit tests for protocol parsing, state derivation, and the setup-page validation

### Deferred

- **Heater control as an ASCOM Switch device.** The protocol supports
  `[GHTM]` / `[SHTM]`; a separate Switch device with three settings
  (`Off`, `On`, `OnWhenFlapOpenOrLed`) could be added in a follow-up.
- **Halt cover.** The FP2 firmware exposes no abort; users wanting this
  should bring it up with Deep Sky Dad.
- **Brightness ramp profiles.** A 2026-07-12 photometric sweep (issue
  #284) found the linear mapping is *not* good enough below raw
  brightness ~250 (see [Brightness linearity](#hardware-constraints)) —
  the driver now floors that range via `cover_calibrator.min_brightness`
  rather than mapping it through a LUT, on the reasoning that no current
  consumer (`calibrator-flats` defaults to `max_brightness` and adjusts
  exposure time, not brightness) has a reason to run the panel that low.
  A full calibration LUT across `0..MaxBrightness` remains future work if
  a use case for low-brightness flats emerges.
- **i18n.** The driver uses plain English log messages; the workspace
  i18n facility (`rusty-photon-i18n`) is only used by services that need
  localised CLI help (e.g. `ppba-driver`).

## Testing Strategy

Follows `docs/skills/testing.md`.

### BDD Tests (Cucumber)

Four feature files cover the MVP behaviour:

- `connection_lifecycle.feature` — connect, disconnect, status after
  disconnect.
- `cover_control.feature` — open, close, state transitions, errors when
  disconnected.
- `calibrator_control.feature` — turn on at brightness, turn off, reject
  brightness above max or below the configured minimum (zero still
  accepted), state after disconnect.
- `setup_pages.feature` — the server page shows the current values; the
  device page shows only its section; a valid save persists, reloads, and
  the page shows the new value after the wait (the scenario pins the bound
  port so the rebuilt server rebinds the same port, and the wait accepts
  only a page whose handler generation differs from the one before the
  save — a page the rebuilt server served — guarding the reload + rebind
  on every CI OS); an invalid
  save names the field and leaves the file untouched; an override-pinned
  field is shown pinned and not persisted; no hash appears on any page; a
  cross-site `POST` is refused; with auth on, the pages answer `401` without
  a credential and render with one; a `server.port` change answers with the
  new URL; `SupportedActions` is empty. The steps are the shared
  `setup_pages_steps!` in `bdd-infra`, so the feature reads the same on
  every driver.

Scenarios spawn the dsd-fp2 binary (built with `--features mock` so
`MockTransportFactory` is wired in place of the real serial transport)
via [`bdd_infra::ServiceHandle`] and drive it through the typed ASCOM
Alpaca `CoverCalibrator` client — same pattern as `ppba-driver` and
`qhy-focuser`. The `World` writes a temp config (`/dev/mock` port, OS-
assigned server port, 100 ms polling interval) and the `bdd_infra::
bdd_main!` macro handles the entry-point + Bazel-runfiles chdir.

Scenarios that need the simulator in a non-default state (e.g. cover
open before exercising `close_cover`) prime it through the client
itself rather than poking `MockState` directly — the spawned binary
gives tests no in-process handle. Scenarios that previously required
bespoke simulator state (e.g. the non-FP2 firmware handshake
rejection) are covered by the in-process unit test
`manager::mock_tests::handshake_rejects_non_fp2_firmware`.

### Unit Tests

- `protocol.rs`: encode/decode every command and response variant;
  malformed input handling (missing `)`, junk before `(`, empty body).
- `device.rs`: `derive_cover_state` / `derive_calibrator_state`
  state-derivation tables (every cell in the CoverState/CalibratorState
  tables above).
- `manager.rs`: brightness validation; the shared-transport crate's
  own integration suite covers refcount + handshake-rollback edge cases.
- `setup.rs`: validation field-errors; normalization; the device section
  the device page edits. The save, the overlay, the pinned-field skip and
  the secret rules are pinned once, in `rusty-photon-setup`.
- `error.rs`: `to_ascom_error()` round-trips per the table in
  [Error Handling](#error-handling); `From<TransportError>` /
  `From<SessionError<DsdFp2Error>>` flattening (centralised here so
  `.map_err(DsdFp2Error::from)?` works at every call site);
  `From<DsdFp2Error> for ASCOMError` so `?` chains land in
  `ASCOMResult<_>` without explicit wrappers.

### ConformU

The service exposes a `conformu = ["mock"]` feature: it gates
`tests/conformu_integration.rs`, and the binary it launches gets the mock
serial port. The test drives ConformU through `bdd_infra::run_conformu` —
the URL-argument verbs, so the run is always ConformU's **full** test set —
with a `bdd_infra::FullRunSettings` that widens only the `Connect()` /
`Disconnect()` budget to 10 s; it self-skips unless `CONFORMU_PATH` names a
ConformU binary and runs nightly in the `conformu.yml` rotation. The same
approach `qhy-focuser` uses.

#### Mock move-settling model

`mock.rs`'s `[SMOV]` applies the target `cover_angle` immediately but
reports the motor running (`[GMOV]` → `(1)`) for exactly one read, then
clears it — mirroring `pa-falcon-rotator`'s `is_moving` (cleared on the
next `FA`). An earlier version resolved `[SMOV]` instantly (position
*and* motor-stopped in the same round trip), which raced the driver's
own optimistic `CoverState = Moving` write (`device.rs::execute_move`):
the very next `while_open` poll tick could observe the mock already
stopped and correct the cache within a few milliseconds of the
optimistic write, and if that landed between ConformU's back-to-back
`CoverState`/`CoverMoving` reads it saw two different snapshots
([#423](https://github.com/rusty-photon/rusty-photon/issues/423)). The
one-shot clear guarantees at least one full `polling_interval` of
stable `Moving` state before the cache can flip, which comfortably
exceeds any HTTP round trip.

## Known Limitations

The driver returns `MethodNotImplementedException` from `HaltCover`
(the FP2 firmware exposes no halt-motion opcode), exactly as the
[ASCOM ICoverCalibratorV2 spec][cc-spec] mandates "if cover movement
cannot be interrupted". ConformU 4.3 incorrectly flagged this
spec-compliant response as an issue
([ASCOMInitiative/ConformU#30](https://github.com/ASCOMInitiative/ConformU/issues/30));
the fix shipped in **ConformU 4.4.0**, which now logs `HaltCover` as OK.
The service therefore declares a `[package.metadata.conformu]` entry and
is included in the rolling ConformU workflow.

[cc-spec]: https://ascom-standards.org/newdocs/covercalibrator.html

## Future Work

- Switch device for heater control (`[SHTM]`).
- Per-filter brightness profiles surfaced to `calibrator-flats` (the
  orchestrator already supports a single `brightness` knob; per-filter
  needs orchestrator changes, not driver changes).
- Optional integration with `sentinel` to surface the heater temperature
  in the dashboard.

## References

- Vendor manual: [DSD-FP2-MANUAL-V3](https://shop.deepskydad.com/software-and-documentation/)
- INDI reference driver: `indilib/indi/drivers/auxiliary/deepskydad_fp.cpp`
- ASCOM CoverCalibrator interface: `ascom-alpaca` crate (pinned commit
  `638429b` on branch `integration` in workspace `Cargo.toml`)
- Sibling services with the same architecture:
  [`qhy-focuser`](qhy-focuser.md), [`ppba-driver`](ppba-driver.md)
- Consumer of this driver:
  [`calibrator-flats`](calibrator-flats.md)

# Deep Sky Dad FP2 Service

## Overview

The `dsd-fp2` service is an ASCOM Alpaca **CoverCalibrator** driver for the
Deep Sky Dad **Flat Panel 2 (FP2)** — a motorised flat-field panel that
combines an electroluminescent light source with a servo-driven cover. The
device speaks a bracketed ASCII protocol over a USB-CDC serial port
(`/dev/ttyACM*` on Linux, vendor/product `2e8a:000a`).

The driver exposes the FP2 as a single ASCOM Alpaca CoverCalibrator device so
that the existing `calibrator-flats` orchestrator (which already consumes
`open_cover` / `close_cover` / `calibrator_on` / `calibrator_off` via `rp`'s
MCP tools) can drive it without any orchestrator changes.

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
|             ▶ tokio_serial::SerialStream                       |
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
  `Fp2SerialTransportFactory` opens a `tokio_serial::SerialStream` and
  wraps it in `SerialFrameTransport` with `terminator = b')'` and a
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

- **Connection**: USB-CDC virtual serial port. Stable path is
  `/dev/serial/by-id/usb-Deep_Sky_Dad_Deep_Sky_Dad_FP2_<serial>-if00`.
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
  a dim request. `config.apply` rejects `min_brightness > max_brightness`
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
   `SharedTransport` increments the refcount and — on the 0→1 transition —
   calls the `TransportFactory::open()` (opens the serial port at 115200
   8N1), constructs a `Connection<Fp2Codec>`, runs the handshake hook,
   then publishes the slot and spawns the while-open task.
2. The handshake hook sends `[GFRM]`, verifies the board is `DeepSkyDad.FP2`,
   and seeds the cached state with a single poll round. Failure (open
   error, non-FP2 firmware, malformed response, IO timeout) propagates
   back through `acquire()`'s `Result` and `SharedTransport`'s
   `RollbackGuard` rolls the refcount back to zero with no slot
   published.
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
    "bind_address": "0.0.0.0",
    "auth": null,
    "tls": null
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

The only escape hatch for changing the id is an explicit edit through the
UI / `config.apply`. To prevent an operator from accidentally blanking the
device's stable identity, `config.apply` **rejects an empty (or
whitespace-only) `cover_calibrator.unique_id`** with a `status:"invalid"`
field error on `cover_calibrator.unique_id` (see [Validation
rules](#validation-rules)). Editing the config file by hand to remove the id
is recoverable: the next startup re-mints a fresh one.

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

## Config Actions

The driver exposes its own configuration over HTTP as two vendor ASCOM
`Action`s, so a host-agnostic UI (the BFF) can read and write config without a
bespoke endpoint. This is the `dsd-fp2` instance of the cross-driver protocol
defined in
[`docs/plans/archive/config-actions.md`](../plans/archive/config-actions.md).

### Supported actions

`supported_actions()` lists:

| Action | `Parameters` | Returns (HTTP 200 body) |
|--------|--------------|--------------------------|
| `config.get` | empty | JSON: effective config (secrets redacted) + override markers |
| `config.apply` | full Config JSON | JSON: apply status + per-field classification |

Unknown actions return `ASCOMError::ACTION_NOT_IMPLEMENTED`. Both actions work
whether or not the device is `Connected` — a wrong `serial.port` must be fixable
without first connecting to the thing the bad port blocks connecting to.

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
falling back to `Config::default()` if the file is somehow absent;
`config.apply` **writes** it, creating parent directories as needed.

### `config.get`

```jsonc
{
  "config": { /* effective Config — see Configuration — with secrets redacted */ },
  "overrides": ["serial.port"]   // fields pinned by --port / --server-port
}
```

- **Effective config** = file config with CLI overrides applied: exactly what the
  running driver is using.
- **Redaction** (hygiene, independent of auth): `server.auth` credential material
  and `server.tls` key material are never emitted in cleartext.
- **`overrides`** lists JSON paths currently pinned by a CLI override (`--port` →
  `serial.port`, `--server-port` → `server.port`). The UI surfaces these as
  not-editable-here; `config.apply` will not persist them.

### `config.apply`

`Parameters` carries the full Config as JSON. The handler:

1. **Parse** as the typed `Config`. Parse failure → ASCOM `INVALID_VALUE` (the
   request was malformed, not the config).
2. **Validate** ranges + semantics (table below). On failure returns **HTTP 200**
   with `{"status":"invalid","errors":[{"path":"serial.baud_rate","msg":"…"}]}`
   and **leaves the file unchanged** — a domain error the BFF renders as
   field-level messages, distinct from a transport/ASCOM error.
3. **Persist** atomically (stage to a unique `NamedTempFile` in the target dir →
   fsync → rename → fsync dir; the random `O_EXCL` temp name avoids collisions
   between concurrent applies and symlink attacks). If the existing config file
   is *present but not valid JSON*, the apply is refused with an
   `INVALID_OPERATION` error rather than treating it as default and overwriting
   (losing) its contents. CLI-override-pinned fields are
   written through from the file's prior value, not the submitted value, and
   listed in `skipped_override[]`. A redacted/sentinel secret (`********`) is
   treated as "unchanged" so a round-tripped form does not blank it — **except**
   when no secret is stored to restore, where the sentinel is rejected as
   `status:"invalid"` (a field error on `server.auth.password_hash`) rather than
   persisted verbatim as the real hash.
4. **Classify** changed fields and **fire the in-process reload** if anything is
   in `reload[]` — *after the response is flushed* (see In-process reload).
5. **Return** HTTP 200 with the classification:

```jsonc
{
  "status": "applying",          // "ok" if nothing needed reload; "invalid" on validation failure
  "applied": [],                 // took effect live (none in Phase 1)
  "reload": ["cover_calibrator.max_brightness"],
  "restart_required": [],        // would need a Sentinel process restart (none in Phase 1)
  "skipped_override": [],        // override-pinned, not persisted
  "persisted_to": "~/.config/rusty-photon/dsd-fp2.json"
}
```

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

#### Field classification (Phase 1)

Every persisted change is classified `reload`: the driver re-reads the file and
rebuilds its server + transport in-process. `applied` (live, no blip) and
`restart_required` exist in the protocol but are unused by `dsd-fp2` in Phase 1;
a later optimisation can move e.g. `cover_calibrator.max_brightness` to `applied`
via a shared config cell. A `server.port` change reloads cleanly here, but
carries a cross-service reference (the BFF's hard-coded driver URL) that Phase 1
does not follow — see the plan's open questions.

### In-process reload

`config.apply` triggers an **in-process reload**, not a process restart.
`main.rs` runs under `ServiceRunner::run_with_reload`: each loop iteration loads
the effective config and builds the server; a `ReloadSignal` held in the device's
config-action state (fired via `notify()`) makes the loop tear the current server
down and rebuild from the new file.

- **Fire-after-response.** The reload is fired from a detached task that yields
  briefly so the `config.apply` response flushes first — the server being torn
  down is the very one serving the request. `status:"applying"` tells the BFF to
  expect a short connection blip and re-`config.get` to confirm.
- **Await the server's own teardown.** The loop passes `start()` a stop future
  that resolves on *either* the service shutdown *or* a reload, and **awaits
  `start()` to completion** rather than dropping it. So `start()`'s teardown
  always runs — gracefully draining HTTP connections and calling
  `manager.transport().shutdown()` to stop the reconnect supervisor and release
  the serial port — before the loop rebuilds. (An earlier design dropped the
  server future on reload, which skipped that teardown and could leak the port +
  supervisor across reloads under the service-lifetime transport model.)
- **Clean HTTP rebind.** The rebuilt server binds the same `server.port` while a
  client's keep-alive connections may still linger on it. The listener is created
  with `SO_REUSEADDR` (`rusty_photon_tls::server::bind_dual_stack`), so the rebind succeeds
  immediately instead of failing with `AddrInUse`. (`SO_REUSEADDR` does not permit
  two live listeners on the port, so it can't mask an "already running" error.)
  A `config_actions.feature` scenario drives a full apply → reload → rebind cycle
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
| `config_actions.rs`   | Config-action protocol: `ConfigAction` enum, request/response envelopes, `validate` / `classify` / `redact` / atomic `save`, override-path tracking |
| `error.rs`            | `DsdFp2Error` (generated by `rusty_photon_driver::driver_error!`: the common transport-driver variants + `to_ascom_error()` + `From<TransportError>` + `From<DsdFp2Error> for ASCOMError`, plus device-specific `MalformedResponse` / `HandshakeFailed`) + `From<SessionError<…>>` |
| `protocol.rs`         | `Command` enum + `RawResponse` body parsers                                                  |
| `codec.rs`            | `Fp2Codec` — `rusty_photon_shared_transport::Codec` impl                                    |
| `transport.rs`        | `Fp2SerialTransportFactory` — opens `tokio_serial::SerialStream`, wraps in `SerialFrameTransport` |
| `manager.rs`          | `FlatPanelManager` over `SharedTransport<Fp2Codec>` + `CachedState` + Hooks (handshake / while_open / teardown) |
| `device.rs`           | `DsdFp2Device` implementing ASCOM `Device + CoverCalibrator`; holds a per-device `Session<Fp2Codec>`; `supported_actions` / `action` dispatch for `config.get` / `config.apply` |
| `mock.rs`             | `MockTransportFactory` + `MockFrameTransport` (feature `mock`); shared `MockState` simulator |
| `lib.rs`              | Public exports, `ServerBuilder`, `BoundServer`                                              |
| `main.rs`             | CLI (clap) + tracing init; lifecycle owned by `rusty-photon-service-lifecycle::ServiceRunner` |

## MVP Scope

### In Scope

- Connect / disconnect over USB-CDC serial
- Cover open / close + state polling (`CoverState` ∈ {`Open`, `Closed`, `Moving`, `Unknown`, `Error`})
- Calibrator on / off + brightness set (`CalibratorState` ∈ {`Off`, `Ready`, `Unknown`, `Error`})
- Brightness read (cached) and `max_brightness = 4096`
- **Config actions** (`config.get` / `config.apply`) with platform-default config
  path, layer-aware persist, validation, secret redaction, and in-process reload
- Mock factory for ConformU and BDD (consumed by both the spawned
  binary and the in-process `manager.rs` mock tests)
- Four BDD feature files: connection_lifecycle, cover_control, calibrator_control,
  config_actions
- Unit tests for protocol parsing, state derivation, and config-action internals

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
- `config_actions.feature` — `supportedactions` lists the config actions;
  `config.get` returns the effective config and marks overrides (over the wire,
  while disconnected); `config.apply` with a valid change returns
  `status:"applying"` with the reload classification; an invalid `baud_rate`
  returns `status:"invalid"` with validation errors; an unknown action returns
  `ACTION_NOT_IMPLEMENTED`; and a valid change is **reloaded end-to-end** — the
  scenario pins the bound port, applies, and then confirms the rebuilt server
  rebinds and serves the new value over the wire (guarding the reload + rebind
  on every CI OS). Secret redaction and file-unchanged-on-invalid are covered by
  the faster in-process unit tests (`device::mock_tests`, `config_actions::tests`).

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
- `config_actions.rs`: validation field-errors; classification of changed
  fields; secret redaction + "absent secret = unchanged" round-trip; atomic
  `save` round-trips and leaves the file unchanged on invalid input;
  layer-aware persist skips CLI-override-pinned fields; XDG path resolution.
- `error.rs`: `to_ascom_error()` round-trips per the table in
  [Error Handling](#error-handling); `From<TransportError>` /
  `From<SessionError<DsdFp2Error>>` flattening (centralised here so
  `.map_err(DsdFp2Error::from)?` works at every call site);
  `From<DsdFp2Error> for ASCOMError` so `?` chains land in
  `ASCOMResult<_>` without explicit wrappers.

### ConformU

The service exposes a `conformu = ["mock", "ascom-alpaca/test"]` feature.
`tests/conformu_integration.rs` launches the binary against a mock port
and points ConformU at it. The same approach `qhy-focuser` uses.

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

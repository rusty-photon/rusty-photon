# QHY Q-Focuser Service

## Overview

The `qhy-focuser` service is an ASCOM Alpaca Focuser driver for the QHY Q-Focuser (EAF - Electronic Auto Focuser). It communicates with the focuser hardware over a USB-CDC serial port using a JSON-based command/response protocol.

The Q-Focuser is a standalone USB serial device - it does **not** use the QHYCCD camera SDK. It has its own USB connection and protocol independent of any QHY camera.

## Architecture

The service is built on `rusty-photon-shared-transport`. The shared crate
owns the refcount, slot, command-lock, and poll-task lifetime; this
service contributes the protocol-specific pieces:

- **Codec**: `QhyCodec` translates `Command` ↔ JSON bytes, dispatches
  decoded responses by `idx`, and matches replies to requests via
  `cmd_id == idx`. The codec sets `max_skip = 5` so the request layer
  can discard up to five unsolicited position frames before erroring
  (the device emits these mid-move). See `src/codec.rs`.
- **Transport factory**: `QhyTransportFactory` opens a `tokio-serial`
  stream and wraps it in a `SerialFrameTransport` with `b'}'` as the
  frame terminator (responses are flat JSON objects terminated by the
  closing brace). See `src/serial.rs`.
- **Manager**: `FocuserManager` wraps `Arc<SharedTransport<QhyCodec>>`
  plus the cached state, and constructs the
  `Hooks { handshake, teardown, while_open }` that the shared transport
  runs across the connection lifecycle. The handshake seeds firmware,
  position, and temperature; the `while_open` poll loop refreshes
  position + temperature on the configured interval. See `src/manager.rs`.
- **Protocol layer**: JSON command serialization and value-level
  response parsers (`src/protocol.rs`).
- **ASCOM device**: `QhyFocuserDevice` holds an
  `Arc<RwLock<Option<Session<QhyCodec>>>>` — the session existing **is**
  the "Connected" state, so the `requested_connection` bool that the
  legacy driver kept separately is gone by construction. Implements
  `Device` + `Focuser` traits (`src/focuser_device.rs`).
- **Mock mode**: `MockQhyTransportFactory` implements `TransportFactory`
  directly and runs an in-memory Q-Focuser state machine. Feature-gated
  on `mock` for binaries, and `#[cfg(any(feature = "mock", test))]` so
  unit and BDD tests can both drive the canonical mock. See `src/mock.rs`.
- **Server builder**: Configures the factory, builds the manager, and
  starts the ASCOM Alpaca server (`src/lib.rs`).

## Hardware Constraints

- **Connection**: USB (presents as a virtual serial/COM port via USB-CDC)
- **Receive buffer**: 128 bytes (`USB_CDC_RX_LEN`) — commands must stay within this limit
- **Stepper motor** with configurable hold current and power-down mode
- **Position range**: -64,000 to +64,000 (max configurable up to 2,000,000)
- **Speed scale**: 0 (fastest) to 8 (slowest)
- **Sensors**: External temperature, chip temperature, input voltage
- **Voltage threshold**: 11.5V — hold force settings are only available above this voltage

## Protocol Reference

The Q-Focuser uses JSON objects over serial. Commands include a `cmd_id` field; responses echo the ID in an `idx` field.

| cmd_id | Command | Parameters | Response Fields | Notes |
|--------|---------|------------|-----------------|-------|
| 1 | GetVersion | — | firmware_version, board_version | |
| 2 | RelativeMove | dir, step | — | dir: 1=inward, -1=outward |
| 3 | Abort | — | — | Stops current movement |
| 4 | ReadTemperature | — | o_t, c_t (÷1000→°C), c_r (÷10→V) | |
| 5 | GetPosition | — | pos | Returns current position |
| 6 | AbsoluteMove | tar | — | Moves to absolute position |
| 7 | SetReverse | rev (0/1) | — | Reverses motor direction |
| 11 | SyncPosition | init_val | — | Sets position counter without moving |
| 13 | SetSpeed | speed (0-8) | — | 0 = fastest |
| 16 | SetHoldCurrent | ihold, irun | — | Motor current (0-31 each) |
| 19 | SetPdnMode | pdn_d | — | Motor power-down mode |

**Not implemented:** cmd_id 12 (SetHoldForce) controls motor holding torque when idle. It exists in the INDI reference driver but is not currently implemented in this service.

Responses are JSON objects terminated by `}` (no newline). Commands are sent as raw JSON without any terminator.

## ASCOM Focuser Mapping

| ASCOM Property/Method | Implementation |
|------------------------|----------------|
| Absolute | `true` (always) |
| IsMoving | Cached state, detected via position polling |
| MaxIncrement | From config `max_step` |
| MaxStep | From config `max_step` |
| Position | Cached from polling (i64 → i32 cast) |
| StepSize | NOT_IMPLEMENTED |
| TempComp | `false` (not supported) |
| TempCompAvailable | `false` |
| Temperature | Outer temperature from polling |
| Halt | Sends Abort command (cmd 3) |
| Move | Validates 0..max_step, sends AbsoluteMove (cmd 6) |

## Configuration

```json
{
  "serial": {
    "port": "/dev/ttyACM0",
    "baud_rate": 9600,
    "polling_interval": "1s",
    "timeout": "2s"
  },
  "server": {
    "port": 11113,
    "bind_address": "0.0.0.0",
    "auth": {
      "username": "observatory",
      "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$..."
    }
  },
  "focuser": {
    "name": "QHY Q-Focuser",
    "unique_id": "",
    "description": "QHY Q-Focuser (EAF) Stepper Motor Controller",
    "enabled": true,
    "max_step": 64000,
    "speed": 0,
    "reverse": false
  }
}
```

`unique_id` is optional and may be omitted or left empty: the service mints
a UUIDv4 on first run and persists it (see [Device identity
(UniqueID)](#device-identity-uniqueid) below).

The `server` block is the shared `AlpacaServerConfig` from
`crates/rusty-photon-server-config` (see ADR-016): `port`, `bind_address`
(default `0.0.0.0`), optional `discovery_port`, and optional `tls`/`auth`.
Absent `tls`/`auth` means plain, unauthenticated HTTP.

Every block (`Config` and each nested config struct) rejects unknown keys at
deserialize (`deny_unknown_fields`), so a typo or a key removed by a schema
change fails loudly at load instead of being silently ignored.

### Device identity (UniqueID)

The focuser's ASCOM `UniqueID` is **minted on first run** rather than shipped
as a hardcoded literal. On startup the service resolves the config path (the
`--config` path if given, otherwise the platform default — e.g.
`~/.config/rusty-photon/qhy-focuser.json` on Linux,
`%PROGRAMDATA%\rusty-photon\qhy-focuser.json` on Windows) via
`rusty_photon_config::resolve_and_init` — the shared bootstrap, called with
the identity pointer `/focuser/unique_id`. That bootstrap:

- mints a spec-compliant UUIDv4 for the `unique_id` if it is absent, empty, or
  not a string, and **never overwrites** an id that already holds a non-empty
  value (so the identity is stable across restarts);
- persists the result atomically, operating on the on-disk file only — a
  transient `--port` / `--server-port` override is never baked into the file;
- **writes the config file if it is absent**, scaffolding it from the defaults
  before minting the id. First run therefore creates the config file at the
  resolved path when none exists.

Because the id is durable once written, set an explicit `unique_id` in the
config only when you need to pin a known value (e.g. migrating an existing
installation); otherwise leave it empty and let the service generate one.

### CLI Arguments

| Argument | Description |
|----------|-------------|
| `-c, --config` | Path to configuration file |
| `--port` | Serial port path (overrides config) |
| `--server-port` | Server port (overrides config) |
| `-l, --log-level` | Log level: trace, debug, info, warn, error |
| `--service` | Hidden: run as a Windows service (passed by the Windows service control manager; no-op on other platforms) |

`qhy-focuser doctor [--config <file>] [--json]` diagnoses this service's own
config read-only without starting it — see
[doctor.md §Per-service doctors](doctor.md). Top-level flags cannot be
combined with the subcommand (the mixed form would silently ignore them).

### Config actions

The focuser exposes its configuration over HTTP as the vendor ASCOM actions
`config.get` / `config.apply` / `config.schema`, the cross-driver protocol
documented in [`config-actions.md`](config-actions.md) and implemented generically
in `rusty_photon_config::actions`. `config_actions.rs` supplies only the
driver-specific half via `ConfigurableDriver for QhyFocuserDriver`:

- **Secrets redacted on read / carried forward on apply:** `/server/auth/password_hash`.
- **Locked (identity) field:** `focuser.unique_id` — the driver owns and mints it;
  the UI renders it read-only behind an "unlock to edit" escape hatch.
- **Hard read-only fields:** `server.port` (the BFF cannot follow a port rebind)
  and `focuser.enabled` (disabling tears down the device the actions live on).
- **CLI-override-pinned fields:** `serial.port` (`--port`) and `server.port`
  (`--server-port`) are reported in `config.get`'s `overrides[]` and never
  persisted by `config.apply`.

A `config.apply` that changes any field persists atomically, returns
`status:"applying"`, and fires the in-process reload (`main.rs` runs under
`ServiceRunner::with_reload().run_with_reload(...)`): the loop tears the old
server down — draining HTTP and releasing the serial port — and rebuilds from the
freshly-persisted file, rebinding the same port.

## Module Structure

| Module | Description |
|--------|-------------|
| `codec.rs` | `QhyCodec` (frame ↔ typed response) + error mapping |
| `config.rs` | Configuration types, loading, `CliOverrides`, `load_effective_config` |
| `config_actions.rs` | `ConfigurableDriver` impl: validation, secrets, editability tiers |
| `error.rs` | Error types with ASCOM error mapping |
| `focuser_device.rs` | ASCOM Device + Focuser trait implementation; config-action dispatch |
| `lib.rs` | Module declarations, ServerBuilder (config source + reload signal) |
| `main.rs` | CLI entry point; `run_with_reload` loop owned by `rusty-photon-service-lifecycle::ServiceRunner` |
| `manager.rs` | `FocuserManager` + handshake / poll-loop hooks |
| `mock.rs` | Mock transport (feature-gated for binaries; always on under `cfg(test)`) |
| `protocol.rs` | JSON command serialization + response parsers |
| `serial.rs` | `QhyTransportFactory` over tokio-serial |

## Testing

- **Unit tests**: Protocol serialization, config, error types, response
  parsing, codec encode/decode/matches, manager behaviour through the
  mock transport factory (in `src/` as `#[cfg(test)]` modules). Race /
  refcount / rollback / while-open lifecycle invariants are tested once
  for everyone in `rusty-photon-shared-transport`'s own integration test
  suite — they are not duplicated here.
- **BDD tests** (cucumber-rs): Device connection lifecycle, metadata,
  readings, movement control, and background polling — all using the
  mock transport infrastructure
- **Server tests**: Server startup with mock feature (`test_lib.rs`,
  feature-gated)
- **ConformU**: ASCOM Alpaca compliance testing (requires ConformU
  installation)

```bash
# Run all tests
bazel test //services/qhy-focuser/...

# Run BDD tests specifically
bazel test --test_tag_filters=bdd //services/qhy-focuser/...

# Run server tests (mock-feature server startup)
bazel test //services/qhy-focuser:test_lib

# Run ConformU compliance tests
bazel test //services/qhy-focuser:conformu_integration

# Run in mock mode
cargo run -p qhy-focuser --features mock
```

## Connection Lifecycle

1. ASCOM client calls `set_connected(true)`.
2. The device acquires a `Session<QhyCodec>` from `SharedTransport`. On
   the 0→1 transition the shared transport opens the serial port via
   `QhyTransportFactory`, runs the handshake hook, then spawns the
   `while_open` poll task.
3. Handshake: GetVersion → SetSpeed → GetPosition → ReadTemperature, all
   issued through `Connection::request` so they go through the same
   request-arbitration lock as steady-state commands.
4. Background polling refreshes position + temperature at the configured
   interval.
5. Move detection: polling compares position to target and clears
   `is_moving` when reached (`is_moving` also force-refreshes position
   on the device's session so the ASCOM property doesn't have to wait
   up to one polling interval).
6. On disconnect, the device calls `Session::close().await`: the
   shared transport cancels the poll task, runs the (currently noop)
   teardown hook, and closes the underlying serial port.

**Failure recovery.** If `factory.open()` or the handshake hook errors
on the 0→1 transition, the shared transport's `RollbackGuard` rolls the
refcount back, drops the connection (closing the underlying port), and
returns `Err` from `acquire()`. A subsequent `set_connected(true)`
re-enters the first-connection path and re-attempts open + handshake
from scratch — the device does not wedge on a transient failure
(unplugged USB during handshake, slow-to-boot firmware, bad serial
path, etc.). This bug class is now eliminated structurally by the
shared crate (issue #258 closes for qhy-focuser with this migration).

## References

- **INDI QHY Focuser Driver** (reference implementation used for protocol reverse-engineering): [qhy_focuser.cpp](https://github.com/indilib/indi-3rdparty/blob/master/indi-qhy/qhy_focuser.cpp), [qhy_focuser.h](https://github.com/indilib/indi-3rdparty/blob/master/indi-qhy/qhy_focuser.h)
- **Q-Focuser Product Page**: [qhyccd.com/q-focuser](https://www.qhyccd.com/q-focuser/)

**Note:** The QHYCCD camera SDK functions `SetQHYCCDFocusSetting()` and `QHYCCD_3A_AUTOFOCUS` (control ID 40) are camera-side autofocus features — they control the camera's focus ROI for autofocus algorithms, not the physical Q-Focuser motor. The Q-Focuser is architecturally independent from any QHY camera.

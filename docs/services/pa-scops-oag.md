# Pegasus Astro Scops OAG Service

## Overview

The `pa-scops-oag` service is an ASCOM Alpaca Focuser driver for the
**Pegasus Astro Scops OAG** — a motorized off-axis guider whose stepper motor
focuses the guide camera. The motor is exposed as a standard absolute-position
ASCOM Focuser. The driver talks to the controller over its FTDI USB virtual
serial port using the Pegasus DMFC/Scops ASCII line protocol.

The Scops OAG is a standalone USB serial device (FTDI FT-X, USB id `0403:6015`).
It has **no temperature sensor**, so temperature and temperature-compensation
properties are reported as unsupported.

## Architecture

The service is built on `rusty-photon-shared-transport`. The shared crate owns
the refcount, slot, command-lock arbitration, poll-task lifetime, and the
reconnect supervisor; this service contributes the protocol-specific pieces:

- **Codec**: `ScopsCodec` translates `Command` ↔ LF-terminated ASCII frames and
  decodes incoming frames into a typed `ScopsResponse` by prefix. Every command
  produces exactly one response frame (no unsolicited frames), so
  `Codec::matches` only enforces command→response-shape pairing and
  `Codec::max_skip` keeps its default of 0. See `src/codec.rs`.
- **Transport factory**: `ScopsTransportFactory` opens a `tokio-serial` stream
  at 19200 8N1 and wraps it in a `SerialFrameTransport` with `b'\n'` as the
  frame terminator. See `src/serial.rs`.
- **Manager**: `FocuserManager` wraps `Arc<SharedTransport<ScopsCodec>>` plus the
  cached state. It constructs the `Hooks { handshake, on_last_disconnect,
  shutdown, while_open }` the shared transport runs across the connection
  lifecycle. The handshake verifies the `#` → `OK_SCOPS` identity and seeds the
  cache from one `A` status report; the `while_open` poll loop refreshes
  position + moving state from `A` on the configured interval. See `src/manager.rs`.
- **Protocol layer**: ASCII command serialization and the `A`-report parser
  (`src/protocol.rs`).
- **ASCOM device**: `ScopsFocuserDevice` holds an
  `Arc<RwLock<Option<Session<ScopsCodec>>>>` — the session existing **is** the
  "Connected" state. Implements `Device` + `Focuser` (`src/focuser_device.rs`).
- **Mock mode**: `MockScopsTransportFactory` implements `TransportFactory`
  directly and runs an in-memory Scops state machine. Feature-gated on `mock`
  for binaries, and `#[cfg(any(feature = "mock", test))]` so unit and BDD tests
  both drive the canonical mock. See `src/mock.rs`.
- **Server builder**: configures the factory, runs the eager startup handshake
  (`transport().start()`), and starts the ASCOM Alpaca server (`src/lib.rs`).

## Hardware Constraints

- **Connection**: USB FTDI FT-X virtual serial port (`/dev/ttyUSB*` on Linux, a
  COM port on Windows). USB id `0403:6015`, product string `Scops OAG`.
- **Serial settings**: **19200 baud, 8N1, no flow control.** This is the
  documented Pegasus DMFC rate and is required by this unit — empirically the
  device does **not** respond at 9600 (the only reply came at 19200). Baud is
  a config field defaulting to 19200.
- **Line terminator**: commands are terminated with LF (`\n`) only — **never**
  append `\r`. Responses are terminated with `\r\n`; the codec trims the
  trailing `\r`.
- **Stepper motor**, absolute position counter (unsigned integer ticks).
- **Position limit**: there is **no firmware max-position command**; the upper
  bound is software-enforced by the driver via `max_step`. The official Pegasus
  Astro software (Unity, Windows) enforces a travel range of **0–22000** for the
  Scops OAG, so `max_step` defaults to `22000`.
- **No temperature sensor.** The `A` report carries a temperature *slot* for
  protocol compatibility with the DMFC family; on this unit it reads `0` and is
  ignored by the driver.
- **No usable reverse / backlash control.** The DMFC `N:` (reverse) and `C:`
  (backlash) set-commands are **rejected with `ERR:`** on Scops firmware 1.2, and
  ASCOM `IFocuserV4` has no reverse or backlash member, so the driver does not
  issue them. The reverse/encoder/backlash fields of the `A` report are read-only
  status and are not surfaced.

## Protocol Reference

ASCII commands over 19200-8N1 serial. Commands are LF-terminated; responses are
CRLF-terminated (the codec trims `\r`). Every command returns exactly one
response frame. Source command family: the Pegasus DMFC serial command table
and the INDI `pegasus_scopsoag` driver; the table below records what was
confirmed on the reference unit (firmware 1.2).

| Purpose | Command | Wire (sent) | Response | Moves motor |
|---------|---------|-------------|----------|-------------|
| Identify / handshake | `Handshake` | `#\n` | `OK_SCOPS\r\n` | no |
| Status report (poll) | `Status` | `A\n` | `OK_SCOPS:<ver>:<motor>:<temp>:<pos>:<moving>:<led>:<rev>:<enc>:<backlash>\r\n` | no |
| Move to absolute position | `MoveAbsolute(pos)` | `M:<pos>\n` | `M:<pos>\r\n` (echo) | **yes** |
| Sync position (no move) | `SyncPosition(pos)` | `W:<pos>\n` | `W:<pos>\r\n` (echo) | no |
| Halt | `Halt` | `H\n` | `0\r\n` | yes |

### `A` status report fields

`OK_SCOPS:1.2:1:0:22000:0:1:0:1:0` decodes (10 colon-delimited fields) as:

| # | Field | Meaning | Driver use |
|---|-------|---------|-----------|
| 1 | status token | `OK_SCOPS` | identity check |
| 2 | firmware version | e.g. `1.2` | cached, surfaced in `driver_info`-adjacent logging |
| 3 | motor type | `1` = stepper | ignored |
| 4 | temperature | placeholder (`0` on this unit; no sensor) | ignored |
| 5 | position | absolute ticks (e.g. `22000`) | cached → ASCOM `Position` |
| 6 | is moving | `0` idle / `1` moving | cached → ASCOM `IsMoving` |
| 7 | LED | `0`/`1` | ignored |
| 8 | reverse | `0`/`1` | ignored (read-only) |
| 9 | encoder | `0`/`1` | ignored |
| 10 | backlash | steps (`0` = off) | ignored (read-only) |

Commands not issued by this driver: `M:`/`W:` are sent in the clean Pegasus form
without the trailing `d` byte that the INDI driver's `snprintf("%ud")` emits;
the firmware tolerates the trailing `d` but the manufacturer form is canonical.
`N:` (reverse) and `C:` (backlash) are intentionally never sent — the device
rejects them and ASCOM has no matching property.

## ASCOM Focuser Mapping

| ASCOM Property/Method | Implementation |
|-----------------------|----------------|
| Absolute | `true` (always) |
| Position | Cached from the `A` report (checked i64 → i32 conversion; a report beyond the i32 range is surfaced as an error, never wrapped). The eager startup handshake seeds the cache before the HTTP listener binds, so it is available immediately after connect; polling keeps it fresh. `INVALID_OPERATION` only in the unseeded-cache edge case (never in normal startup) |
| IsMoving | Cached from `A` field 6; force-refreshed via `A` when a move is in flight |
| MaxStep | From config `max_step` |
| MaxIncrement | From config `max_step` (a single absolute move can span full travel) |
| Move | Validates `0..=max_step`, sends `M:<pos>` |
| Halt | Sends `H` |
| StepSize | `NOT_IMPLEMENTED` (no microns/step figure for the OAG focuser) |
| TempComp | `false` |
| TempCompAvailable | `false` |
| Temperature | `NOT_IMPLEMENTED` (no sensor) |
| InterfaceVersion | `4` |

The single substantive divergence from `qhy-focuser` is `Temperature`: the
Scops OAG has no probe, so it returns `NOT_IMPLEMENTED` rather than a value, and
`TempCompAvailable`/`TempComp` are `false` — a self-consistent "no temperature
compensation" profile ConformU accepts.

## Configuration

```json
{
  "serial": {
    "port": "/dev/ttyUSB0",
    "baud_rate": 19200,
    "polling_interval": "1s",
    "timeout": "2s"
  },
  "server": {
    "port": 11123,
    "bind_address": "0.0.0.0"
  },
  "focuser": {
    "name": "Pegasus Scops OAG",
    "unique_id": "",
    "description": "Pegasus Astro Scops OAG motorized off-axis guider focuser",
    "enabled": true,
    "max_step": 22000
  }
}
```

`unique_id` is optional and may be omitted or left empty: the service mints a
UUIDv4 on first run and persists it (see [Device identity](#device-identity-uniqueid)).
`max_step` defaults to `22000`, the travel range the official Pegasus Astro
software enforces for the Scops OAG (positions 0–22000).

The `server` block is the shared `AlpacaServerConfig` from
`crates/rusty-photon-server-config` (see ADR-016): `port`, `bind_address`
(default `0.0.0.0`), optional `discovery_port`, and optional `tls`/`auth`.
Absent `tls`/`auth` means plain, unauthenticated HTTP.

`server.discovery_port` (not shown above) is the Alpaca UDP discovery responder
port (opt-in; normally `32227`). Absent/`null` — the default — disables
discovery: many rusty-photon servers on one host would collide on the shared
discovery port, so it is a per-host opt-in for single-driver deployments.

Every block (`Config` and each nested config struct) rejects unknown keys at
deserialize (`deny_unknown_fields`), so a typo or a key removed by a schema
change fails loudly at load instead of being silently ignored.

### Device identity (UniqueID)

The focuser's ASCOM `UniqueID` is **minted on first run** rather than shipped as
a hardcoded literal. On startup the service resolves the config path (the
`--config` path if given, otherwise the platform default — e.g.
`~/.config/rusty-photon/pa-scops-oag.json` on Linux,
`%PROGRAMDATA%\rusty-photon\pa-scops-oag.json` on Windows) via
`rusty_photon_config::resolve_and_init` — the shared bootstrap, called with
the identity pointer `/focuser/unique_id`. It mints a spec-compliant UUIDv4 if the id is
absent/empty, never overwrites a non-empty id, writes the default scaffold if the
file is absent, and persists atomically (the on-disk file only — a transient
`--port`/`--server-port` override is never baked in).

### CLI Arguments

| Argument | Description |
|----------|-------------|
| `-c, --config` | Path to configuration file |
| `--port` | Serial port path (overrides config) |
| `--server-port` | Server port (overrides config) |
| `-l, --log-level` | Log level: trace, debug, info, warn, error |
| `--service` | Hidden: run as a Windows service (passed by the Windows service control manager; no-op on other platforms) |

`pa-scops-oag doctor [--config <file>] [--json]` diagnoses this service's own
config read-only without starting it — see
[doctor.md §Per-service doctors](doctor.md). Top-level flags cannot be
combined with the subcommand (the mixed form would silently ignore them).

### Config actions

The focuser exposes its configuration over HTTP as the vendor ASCOM actions
`config.get` / `config.apply` / `config.schema`, the cross-driver protocol
documented in [`config-actions.md`](config-actions.md) and implemented
generically in `rusty_photon_config::actions`. `config_actions.rs` supplies only
the driver-specific half via `ConfigurableDriver for ScopsFocuserDriver`:

- **Secrets redacted on read / carried forward on apply:** `/server/auth/password_hash`.
- **Locked (identity) field:** `focuser.unique_id` — the driver owns and mints it.
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
| `codec.rs` | `ScopsCodec` (frame ↔ typed response) + error mapping |
| `config.rs` | Configuration types, loading, `CliOverrides`, `load_effective_config` |
| `config_actions.rs` | `ConfigurableDriver` impl: validation, secrets, editability tiers |
| `error.rs` | `ScopsOagError` via `rusty_photon_driver::driver_error!` |
| `focuser_device.rs` | ASCOM Device + Focuser trait implementation; config-action dispatch |
| `lib.rs` | Module declarations, ServerBuilder (config source + reload signal) |
| `main.rs` | CLI entry point; `run_with_reload` loop owned by `rusty-photon-service-lifecycle::ServiceRunner` |
| `manager.rs` | `FocuserManager` + handshake / poll-loop hooks |
| `mock.rs` | Mock transport (feature-gated for binaries; always on under `cfg(test)`) |
| `protocol.rs` | ASCII command serialization + `A`-report parser |
| `serial.rs` | `ScopsTransportFactory` over tokio-serial |

## Connection Lifecycle

The service runs in `ServiceLifetime` mode: `ServerBuilder::build()` calls
`transport().start()` so the port is opened, the handshake runs, and the poll
task spawns **before** the HTTP listener binds (eager hardware validation — a
handshake failure exits the process non-zero rather than advertising a broken
device).

1. `ServerBuilder::build()` runs `transport().start()`:
   - opens the serial port via `ScopsTransportFactory`,
   - handshake hook: `#` → verify `OK_SCOPS`, then `A` → seed cache (position,
     is_moving, firmware),
   - spawns the `while_open` poll task (issues `A` on the configured interval).
2. ASCOM client calls `set_connected(true)` → the device `acquire()`s a session
   (fast refcount-bump; the port is already open).
3. Background polling refreshes position + is_moving from `A`.
4. Move: `move_(pos)` validates `0..=max_step`, sets cache `is_moving = true`
   optimistically, sends `M:<pos>`. The next poll (or a force-refresh from
   `is_moving()`) updates `is_moving`/position from the device's `A` report.
5. Halt: `halt()` sends `H` and clears the cached move state.
6. On `set_connected(false)` the session is released (refcount-bump down; the
   port stays open in `ServiceLifetime` mode). On process shutdown,
   `transport().shutdown()` cancels the poll task and closes the port.

**Failure recovery.** Mid-stream transport errors flip the shared transport into
the `Reconnecting` state and the supervisor re-opens + re-handshakes; live
sessions resume on the new connection on their next request.

## Testing

- **Unit tests** (`#[cfg(test)]` in `src/`): command serialization, `A`-report
  parsing (happy path + every error variant), codec encode/decode/matches,
  config defaults + `config_actions` validation, error conversions, and manager
  behaviour driven through the mock transport factory. Race / refcount /
  rollback / while-open lifecycle invariants are tested once for everyone in
  `rusty-photon-shared-transport`'s own suite — not duplicated here.
- **BDD tests** (cucumber-rs): connection lifecycle, device metadata, movement
  control, focuser readings, background polling, config actions, auth, TLS — all
  driving the spawned `--features mock` binary over the typed Alpaca client.
- **Server tests** (`test_lib.rs`, feature-gated `mock`): server startup +
  device registration.
- **ConformU** (`conformu_integration.rs`, feature-gated `conformu`): ASCOM
  Alpaca Focuser compliance against the mock binary.

```bash
cargo test -p pa-scops-oag --features mock
cargo test -p pa-scops-oag --test bdd --features mock
cargo test -p pa-scops-oag --features conformu --test conformu_integration -- --nocapture
cargo run  -p pa-scops-oag --features mock
```

## MVP scope

**In scope:** absolute Move, Halt, Position, IsMoving, the connection lifecycle,
config actions, auth, and TLS — the full ASCOM `IFocuserV4` surface the Scops
OAG can back.

**Deferred / out of scope:** reverse and backlash control (rejected by the
firmware and absent from `IFocuserV4`); temperature and temperature compensation
(no sensor); LED, encoder, and motor-speed controls (not part of the Focuser
contract). The `A` report's status-only fields for these are read but not
surfaced.

## References

- **INDI Pegasus Scops OAG driver** (protocol reference):
  [pegasus_scopsoag.cpp](https://github.com/indilib/indi/blob/master/drivers/focuser/pegasus_scopsoag.cpp),
  [dmfc.cpp](https://github.com/indilib/indi/blob/master/drivers/focuser/dmfc.cpp)
- **Pegasus DMFC serial command table**:
  [dmfc-serial-command-table](https://pegasusastro.com/products/dmfc/dmfc-serial-command-table/)
- **Scops OAG product page**: [scops-oag](https://pegasusastro.com/products/scops-oag/)
- Sibling services: [`qhy-focuser.md`](qhy-focuser.md) (ASCOM Focuser template),
  [`falcon-rotator.md`](falcon-rotator.md) (Pegasus ASCII serial protocol).

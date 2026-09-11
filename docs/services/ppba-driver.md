# PPBA Switch Driver

ASCOM Alpaca Switch driver for the Pegasus Astro Pocket Powerbox Advance Gen2 (PPBA).

## Overview

This service exposes the PPBA device as an ASCOM Alpaca Switch device, allowing control of power outputs, dew heaters, and monitoring of device sensors through the standard ASCOM Switch interface.

## Device Protocol

The PPBA communicates via serial at 9600 baud, 8N1, with newline-terminated commands.

### Commands

| Command | Description | Response |
|---------|-------------|----------|
| `P#` | Ping/status check | `PPBA_OK` |
| `PV` | Firmware version | `n.n.n` |
| `PA` | Full status | `PPBA:voltage:current:temp:humidity:dewpoint:quad:adj:dewA:dewB:autodew:warn:pwradj` |
| `PS` | Power statistics | `PS:averageAmps:ampHours:wattHours:uptime_ms` |
| `P1:b` | Set quad 12V output (0/1) | `P1:b` |
| `P2:n` | Set adjustable output (0/1) | `P2:n` |
| `P3:nnn` | Set DewA PWM (0-255) | `P3:nnn` |
| `P4:nnn` | Set DewB PWM (0-255) | `P4:nnn` |
| `PU:b` | USB2 hub control (0/1) | `PU:b` |
| `PD:b` | Auto-dew enable (0/1) | `PD:b` |

## Switch Mapping

### Controllable Switches (CanWrite = true)

| ID | Name | Type | Min | Max | Step | Command | Notes |
|----|------|------|-----|-----|------|---------|-------|
| 0 | Quad 12V Output | Boolean | 0 | 1 | 1 | `P1:b` | |
| 1 | Adjustable Output | Boolean | 0 | 1 | 1 | `P2:b` | |
| 2 | Dew Heater A | PWM | 0 | 255 | 1 | `P3:nnn` | Read-only when auto-dew enabled |
| 3 | Dew Heater B | PWM | 0 | 255 | 1 | `P4:nnn` | Read-only when auto-dew enabled |
| 4 | USB Hub | Boolean | 0 | 1 | 1 | `PU:b` | |
| 5 | Auto-Dew | Boolean | 0 | 1 | 1 | `PD:b` | |

**Note:** See [Auto-Dew Behavior](#auto-dew-behavior) for important information about the interaction between auto-dew and manual dew heater control.

### Read-Only Switches - Power Statistics (CanWrite = false)

| ID | Name | Type | Min | Max | Step | Source |
|----|------|------|-----|-----|------|--------|
| 6 | Average Current | Amps | 0 | 20 | 0.01 | `PS` command |
| 7 | Amp Hours | Ah | 0 | 9999 | 0.01 | `PS` command |
| 8 | Watt Hours | Wh | 0 | 99999 | 0.1 | `PS` command |
| 9 | Uptime | Hours | 0 | 99999 | 0.01 | `PS` command |

### Read-Only Switches - Sensor Data (CanWrite = false)

| ID | Name | Type | Min | Max | Step | Source |
|----|------|------|-----|-----|------|--------|
| 10 | Input Voltage | Volts | 0 | 15 | 0.1 | `PA` command |
| 11 | Total Current | Amps | 0 | 20 | 0.01 | `PA` command |
| 12 | Temperature | °C | -40 | 60 | 0.1 | `PA` command |
| 13 | Humidity | % | 0 | 100 | 1 | `PA` command |
| 14 | Dewpoint | °C | -40 | 60 | 0.1 | `PA` command |
| 15 | Power Warning | Boolean | 0 | 1 | 1 | `PA` command |

**Total: 16 switches** (MaxSwitch = 16)

### Operator labels

A port number is what the box knows; what is plugged into it is what the
operator knows. `switch.labels` carries that fact across: it replaces the
built-in name of any switch that corresponds to a connector. An absent or
empty block leaves every name exactly as the tables above list them.

```json
"switch": {
  "name": "Pegasus PPBA Switch",
  "labels": {
    "Quad 12V Output": "Mount and camera rail",
    "Adjustable Output": "Dew controller",
    "USB Hub": "Guide camera hub"
  }
}
```

Two things to know about the shape:

- **Keys are built-in names, not ids.** `"Quad 12V Output"`, not `"0"`. The
  file is then readable without the id table open, and a key naming no
  labellable switch is rejected — which is what makes a typo loud instead of
  silently inert.
- **A label follows its port's telemetry.** The PPBA reports no per-port
  current or overcurrent, so on this box every label governs exactly one
  name. The behaviour is the shared type's, and it is what renames ids 20 and
  27 alongside id 0 on the [UPBv2](upbv2-driver.md#operator-labels), whose
  `PA` does carry per-port telemetry.

Three rules, all enforced when the config is **deserialized** rather than by a
separate validation pass, so a bad map fails at startup — and fails a
`config.apply` — with the offending entry named:

1. **Only ids 0-4 may be labelled**: the quad 12 V output, the adjustable
   output, the two dew heaters and the USB hub. Those are the switches an
   operator plugs equipment into. Auto-Dew (id 5) is writable but is a
   *mode*, not a connector, so it keeps its name alongside the read-only
   rows — a client that saw `Humidity` or `Auto-Dew` renamed would have no
   way to know what it was reading or setting.
2. **A label needs non-whitespace content.** Removing the entry is how a
   switch goes back to its built-in name. `""` or a run of spaces is
   *rejected* rather than quietly meaning the same thing, so a half-finished
   edit fails loudly instead of passing for a deliberate reset.
3. **The 16 names stay unique.** ASCOM clients key on the name, so a label
   that collides — with another label, or with the built-in name of a switch
   left unlabelled — is rejected.

`GetSwitchDescription` is untouched. The description already names the
physical output, so it stays identifiable after the name is replaced:
switch 0 labelled `Mount and camera rail` still describes itself as
"Controls the quad 12V power output".

Labels are an ordinary config field, so `config.apply` edits them and the
service reloads onto the new names. `SetSwitchName` stays `NOT_IMPLEMENTED`:
a name written over the wire would not survive a restart, and the config file
is the one place the mapping is recorded.

The label map is `SwitchLabels` from `crates/rusty-photon-server-config`,
shared with [`upbv2-driver`](upbv2-driver.md#operator-labels) — the two
configs stay parallel because they are the same type, parameterised by each
driver's own switch table.

## Configuration

Configuration is provided via a JSON file:

```json
{
  "serial": {
    "port": "/dev/ttyUSB0",
    "baud_rate": 9600,
    "polling_interval": "5s",
    "timeout": "2s"
  },
  "server": {
    "port": 11112,
    "bind_address": "0.0.0.0",
    "tls": null,
    "auth": {
      "username": "observatory",
      "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$..."
    }
  },
  "switch": {
    "name": "Pegasus PPBA Switch",
    "unique_id": "8f1c3a2e-5b7d-4e9a-9c1f-2a6b8d0e4f31",
    "description": "Pegasus Astro PPBA Gen2 Power Control",
    "enabled": true,
    "labels": { "Quad 12V Output": "Mount and camera rail" }
  },
  "observingconditions": {
    "name": "Pegasus PPBA Weather",
    "unique_id": "1d4e6f80-2c9b-47a3-8e51-7f0a3b5c9d2e",
    "description": "Pegasus Astro PPBA Environmental Sensors",
    "enabled": true,
    "averaging_period": "5m"
  }
}
```

The `server` block is the shared `AlpacaServerConfig` from
`crates/rusty-photon-server-config` (see ADR-016): `port`, `bind_address`
(default `0.0.0.0`), optional `discovery_port`, and optional `tls`/`auth`.
Absent `tls`/`auth` means plain, unauthenticated HTTP.

Every block (`Config` and each nested config struct) rejects unknown keys at
deserialize (`deny_unknown_fields`), so a typo or a key removed by a schema
change fails loudly at load instead of being silently ignored.

### Configuration Options

| Section | Field | Description | Default |
|---------|-------|-------------|---------|
| serial | port | Serial port path | "/dev/ttyUSB0" on Unix, "COM3" on Windows (placeholder — edit to the real port) |
| serial | baud_rate | Baud rate | 9600 |
| serial | polling_interval | Status poll interval (humantime, e.g. `"5s"`, `"500ms"`) | `"5s"` |
| serial | timeout | Serial timeout (humantime) | `"2s"` |
| server | port | HTTP server port | 11112 |
| server | bind_address | Interface to bind (`0.0.0.0` = all interfaces) | "0.0.0.0" |
| server.auth | username | HTTP Basic Auth username (optional) | — |
| server.auth | password_hash | Argon2id password hash (optional) | — |
| switch | name | ASCOM device name for the Switch | "Pegasus PPBA Switch" |
| switch | unique_id | ASCOM `UniqueID` for the Switch (see [Device identity](#device-identity-uniqueid)) | minted UUIDv4 on first run |
| switch | description | Switch description | "Pegasus Astro PPBA Gen2 Power Control" |
| switch | enabled | Whether to register the Switch device | `true` |
| switch | labels | Operator labels for switches 0-4, keyed by built-in name (see [Operator labels](#operator-labels)) | absent — every switch keeps its built-in name |
| observingconditions | name | ASCOM device name for ObservingConditions | "Pegasus PPBA Weather" |
| observingconditions | unique_id | ASCOM `UniqueID` for ObservingConditions (see [Device identity](#device-identity-uniqueid)) | minted UUIDv4 on first run |
| observingconditions | description | ObservingConditions description | "Pegasus Astro PPBA Environmental Sensors" |
| observingconditions | enabled | Whether to register the ObservingConditions device | `true` |
| observingconditions | averaging_period | Sliding-window length for sensor means (humantime) | `"5m"` |

### Device identity (UniqueID)

This driver exposes **two** ASCOM device identities — the Switch and the
ObservingConditions device — each with its own `UniqueID`. ASCOM Alpaca requires
every device's `UniqueID` to be globally unique and stable for the life of the
installation.

On **first run**, each device's `unique_id` is minted as a spec-compliant
UUIDv4 and persisted to the config file via the shared
`rusty_photon_config::resolve_and_init` bootstrap.
Materialization is idempotent and **never overwrites** an id that already holds a
non-empty value — only empty or absent ids are filled — so a device keeps the
same identity across restarts. The defaults for both `unique_id` fields are
therefore the empty string, which signals "mint me on first run".

The config path is resolved from `--config` if given, otherwise from the
platform default (e.g. `~/.config/rusty-photon/ppba-driver.json` on Linux,
`%PROGRAMDATA%\rusty-photon\ppba-driver.json` on Windows). Because identity must be
persisted, **first run now writes the config file if it is absent**, seeding it
with the default scaffold and the two freshly-minted UUIDs. CLI overrides
(`--port`, `--server-port`, `--enable-switch`, `--enable-observingconditions`)
are applied to the in-memory config *after* loading and are never written back
to disk.

### Config actions

Both devices expose the configuration over HTTP as the vendor ASCOM actions
`config.get` / `config.apply` / `config.schema` — the cross-driver protocol in
[`config-actions.md`](config-actions.md), implemented generically in
`rusty_photon_config::actions`. `config_actions.rs` supplies the driver-specific
half (`ConfigurableDriver for PpbaDriver`) **and** the shared `dispatch` both
`switch_device.rs` and `observingconditions_device.rs` delegate to, so an apply
on either device operates on the one full driver config and fires the one reload
(`ReloadSignal::notify` coalesces).

- **Secret redacted / carried forward:** `/server/auth/password_hash`.
- **Locked (identity) fields:** `switch.unique_id`, `observingconditions.unique_id`.
- **Hard read-only fields:** `server.port`, `switch.enabled`,
  `observingconditions.enabled` (disabling a device tears down the endpoint the
  config actions live on).
- **CLI-override-pinned:** `serial.port` (`--port`), `server.port`
  (`--server-port`), `switch.enabled` (`--enable-switch`),
  `observingconditions.enabled` (`--enable-observingconditions`) — reported in
  `config.get`'s `overrides[]` and never persisted by `config.apply`.

A `config.apply` that changes a field persists atomically, returns
`status:"applying"`, and fires the in-process reload; `main.rs` runs under
`ServiceRunner::with_reload().run_with_reload(...)`, which tears the old server
down (releasing the shared serial port) and rebuilds from the freshly-persisted
file, rebinding the same port.

## Usage

### Starting the Service

```bash
# With configuration file
cargo run -p ppba-switch -- -c config.json

# With command-line overrides
cargo run -p ppba-switch -- --port /dev/ttyUSB1 --server-port 11113

# With debug logging
cargo run -p ppba-switch -- -c config.json -l debug
```

### CLI Options

| Option | Description |
|--------|-------------|
| `-c, --config <FILE>` | Path to configuration file |
| `--port <PORT>` | Serial port (overrides config) |
| `--server-port <PORT>` | Server port (overrides config) |
| `-l, --log-level <LEVEL>` | Log level (trace, debug, info, warn, error) |
| `--service` | Hidden: run as a Windows service (passed by the Windows service control manager; no-op on other platforms) |

`ppba-driver doctor [--config <file>] [--json]` diagnoses this service's own
config read-only without starting it — see
[doctor.md §Per-service doctors](doctor.md). Top-level flags cannot be
combined with the subcommand (the mixed form would silently ignore them).

### Localised CLI help

`ppba-driver`'s `--help` output and the `--log-level` validation error are
translated via Fluent (`crates/rusty-photon-i18n` + `i18n-embed`) — see
[`docs/plans/archive/i18n-cli-spike.md`](../plans/archive/i18n-cli-spike.md). Locale is
resolved at startup, before clap parses arguments. Precedence:

1. `RP_LOCALE`
2. `LC_ALL`, `LC_MESSAGES`, `LANG`
3. OS-reported locale
4. `en` (fallback)

Translation files live under `services/ppba-driver/i18n/{locale}/ppba-driver.ftl`
and are embedded into the binary at compile time. Currently shipped:
`en` (source) and `de` (LLM-bootstrapped, marked `# machine-translated, needs review`).
Unsupported locales fall back to `en`.

Clap's own built-in messages ("Usage:", "Options:", "error: …") remain English
in this spike — translating them is a separate decision tracked in
[`docs/plans/i18n.md`](../plans/i18n.md).

## ASCOM Alpaca API

### Endpoints

The service exposes standard ASCOM Alpaca Switch endpoints:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/v1/switch/0/maxswitch` | GET | Returns 16 |
| `/api/v1/switch/0/canwrite?Id=N` | GET | Check if switch is writable |
| `/api/v1/switch/0/getswitch?Id=N` | GET | Get boolean state |
| `/api/v1/switch/0/setswitch` | PUT | Set boolean state |
| `/api/v1/switch/0/getswitchvalue?Id=N` | GET | Get numeric value |
| `/api/v1/switch/0/setswitchvalue` | PUT | Set numeric value |
| `/api/v1/switch/0/getswitchname?Id=N` | GET | Get switch name |
| `/api/v1/switch/0/getswitchdescription?Id=N` | GET | Get switch description |
| `/api/v1/switch/0/minswitchvalue?Id=N` | GET | Get minimum value |
| `/api/v1/switch/0/maxswitchvalue?Id=N` | GET | Get maximum value |
| `/api/v1/switch/0/switchstep?Id=N` | GET | Get step size |

### Example curl Commands

```bash
# Get max switch count
curl http://localhost:11112/api/v1/switch/0/maxswitch

# Get input voltage (switch 10)
curl "http://localhost:11112/api/v1/switch/0/getswitchvalue?Id=10"

# Turn on quad 12V (switch 0)
curl -X PUT http://localhost:11112/api/v1/switch/0/setswitch \
  -d "Id=0&State=true"

# Set dew heater A to 50% (switch 2, PWM 128)
curl -X PUT http://localhost:11112/api/v1/switch/0/setswitchvalue \
  -d "Id=2&Value=128"
```

## Architecture

### Module Structure

```
ppba-driver/
├── src/
│   ├── lib.rs                        # Crate root, ServerBuilder
│   ├── main.rs                       # CLI entry point; lifecycle owned by rusty-photon-service-lifecycle::ServiceRunner
│   ├── config.rs                     # Configuration types
│   ├── error.rs                      # Error types
│   ├── switch_device.rs              # ASCOM Switch implementation
│   ├── observingconditions_device.rs # ASCOM ObservingConditions implementation
│   ├── manager.rs                    # PpbaManager (cached state + hooks for SharedTransport)
│   ├── codec.rs                      # PpbaCodec (Codec impl for rusty-photon-shared-transport)
│   ├── protocol.rs                   # PPBA command/response handling
│   ├── serial.rs                     # PpbaTransportFactory (tokio-serial → SerialFrameTransport)
│   ├── mock.rs                       # MockPpbaTransportFactory (feature-gated)
│   ├── switches.rs                   # Switch definitions
│   └── mean.rs                       # Sliding window sensor mean
├── tests/
│   ├── bdd.rs                        # BDD entry point (cucumber-rs)
│   ├── bdd/
│   │   ├── world.rs                  # PpbaWorld struct + helpers
│   │   └── steps/
│   │       ├── mod.rs
│   │       ├── infrastructure.rs     # ServiceHandle (from bdd-infra), config helpers
│   │       ├── connection_steps.rs   # Connect/disconnect via HTTP
│   │       ├── switch_metadata_steps.rs
│   │       ├── switch_control_steps.rs
│   │       ├── switch_error_steps.rs
│   │       ├── sensor_steps.rs
│   │       ├── oc_steps.rs           # ObservingConditions steps
│   │       └── server_steps.rs       # Server registration
│   ├── features/
│   │   ├── connection_lifecycle.feature
│   │   ├── switch_metadata.feature
│   │   ├── switch_control.feature
│   │   ├── switch_errors.feature
│   │   ├── sensor_readings.feature
│   │   ├── observing_conditions.feature
│   │   └── server_registration.feature
│   └── conformu_integration.rs       # ASCOM ConformU compliance tests
│   # Unit and mock-based tests are in src/ as #[cfg(test)] modules
└── examples/
    ├── config-linux.json
    ├── config-macos.json
    └── config-windows.json
```

### Key Design Decisions

1. **Shared transport via `rusty-photon-shared-transport`**: refcounted lifecycle, command-lock arbitration, while-open poll task, and the connect/handshake/teardown sequence all live in the shared crate. `PpbaCodec` plugs in the `P#`/`PA`/`PS` framing and response parsing; `PpbaTransportFactory` opens a `SerialFrameTransport` over `tokio-serial`. Each ASCOM device holds `Option<Session<PpbaCodec>>` — the session existing is the canonical "Connected" state, so the previously-separate "requested" flag can't desync from the underlying transport (issue #251 cannot reoccur).

2. **Background polling**: Once the transport is open, the shared crate's `while_open` hook runs the PPBA poll loop (PA + PS every `polling_interval`) into the shared `CachedState`. Reads are served from the cache; writes refresh on demand.

3. **PWM Values**: Dew heaters use raw 0-255 PWM values matching the device protocol directly. ASCOM clients can use `SetSwitchValue()` with the PWM value.

4. **Synchronous Operations**: The MVP uses synchronous switch operations. Async switch methods are not implemented.

5. **USB Hub Tracking**: USB hub state is tracked separately since it's not included in the `PA` status response.

## Auto-Dew Behavior

The PPBA has a built-in auto-dew feature (switch 5) that automatically calculates and applies optimal PWM values to the dew heaters based on ambient temperature and humidity readings.

### Dynamic Write Protection

This driver implements **dynamic write protection** for dew heater switches (2 & 3) based on the auto-dew state:

**When auto-dew is ENABLED (switch 5 = ON):**
- `CanWrite(2)` and `CanWrite(3)` return `false` (read-only)
- Attempting to write to switches 2 or 3 returns an `INVALID_OPERATION` error
- Error message: "Cannot write to switch X while auto-dew is enabled. Disable auto-dew (switch 5) first."

**When auto-dew is DISABLED (switch 5 = OFF):**
- `CanWrite(2)` and `CanWrite(3)` return `true` (writable)
- Switches 2 & 3 can be written normally

**When disconnected:**
- `CanWrite()` for any switch returns a `NOT_CONNECTED` error (per ASCOM specification)

### State Caching and Refresh Behavior

The driver caches device state to minimize serial communication overhead:

- **Background polling**: Device state is refreshed every `polling_interval` (default: `"5s"`)
- **CanWrite() queries**: Use cached state (may be up to `polling_interval` stale if auto-dew changed externally), except for dew heaters (switches 2 & 3) which refresh the cache if not yet populated to ensure accurate writability reporting
- **SetSwitchValue() for dew heaters**: Refreshes state immediately before validation (always validates against current device state)
- **After successful writes**: State is refreshed immediately to reflect the change
- **External changes**: Auto-dew changes made by other clients or via serial are detected within the polling interval

For tighter synchronization with external changes, reduce `polling_interval` in the configuration. However, note that very short intervals (< 1s) increase serial communication overhead.

### Manual Dew Heater Control

To manually control dew heaters:

```bash
# 1. First, disable auto-dew (switch 5)
curl -X PUT http://localhost:11112/api/v1/switch/0/setswitch \
  -d "Id=5&State=false"

# 2. Now manual dew heater control will work
curl -X PUT http://localhost:11112/api/v1/switch/0/setswitchvalue \
  -d "Id=2&Value=128"
```

If you attempt to set a dew heater while auto-dew is enabled, you'll receive an error:

```bash
# This will fail with INVALID_OPERATION error:
curl -X PUT http://localhost:11112/api/v1/switch/0/setswitchvalue \
  -d "Id=2&Value=128"
# Error: "Cannot write to switch 2 while auto-dew is enabled. Disable auto-dew (switch 5) first."
```

### Client Recommendations

For robust client applications:

1. **Always connect first**: `CanWrite()` requires an active connection
2. **Check CanWrite() before writing**: Query `CanWrite(id)` to determine if a switch is currently writable
3. **Handle write errors gracefully**: Catch `INVALID_OPERATION` errors when writing to dew heaters
4. **Update UI on auto-dew changes**: If your UI allows controlling both auto-dew and manual heaters, update the dew heater controls' enabled/disabled state when auto-dew changes

Example client flow:

```python
# Connect to device
device.Connected = True

# Check if dew heater A is writable
if device.CanWrite(2):
    # Write is allowed
    device.SetSwitchValue(2, 128)
else:
    # Dew heater is read-only (auto-dew is probably ON)
    print("Cannot write to dew heater while auto-dew is enabled")
```

### ConformU Testing

When running ASCOM ConformU compliance tests against real hardware, auto-dew must be disabled first for the dew heater tests (switches 2 and 3) to pass.

## Testing

```bash
# Run unit tests only
bazel test //services/ppba-driver:ppba-driver_unit_test

# Run BDD tests (spawns the mock binary with MockSerialPortFactory)
bazel test --test_tag_filters=bdd //services/ppba-driver/...

# Run all tests (unit + BDD)
bazel test //services/ppba-driver/...

# Run a specific unit test module (inline in src/)
bazel test //services/ppba-driver:ppba-driver_unit_test --test_arg=protocol::tests

# Run ConformU compliance test (requires ConformU installed)
bazel test //services/ppba-driver:conformu_integration
```

BDD tests use cucumber-rs with feature files in `tests/features/`. Tests spawn the actual ppba-driver binary as a subprocess (with `--features mock` for the mock serial port) and communicate via ASCOM Alpaca HTTP REST API, testing the full stack from config loading through HTTP routing to device logic.

### ConformU Compliance Testing

The driver includes ASCOM ConformU compliance tests that verify conformance to the ASCOM Switch and ObservingConditions interface specifications. These tests run in CI via the `conformu.yml` workflow.

**Performance optimization**: ConformU uses configurable delays between Switch read/write operations. The test uses a complete ConformU settings file with reduced delays:
- `SwitchReadDelay`: 50ms (default: 500ms)
- `SwitchWriteDelay`: 100ms (default: 3000ms)

This reduces test time from ~8 minutes to ~35 seconds per platform.

**Important**: ConformU requires a complete settings file with all required properties. Partial settings files (with only the Switch delays) are ignored and overwritten with defaults.

#### Running ConformU Against Real Hardware

To run ConformU compliance tests against the actual PPBA hardware on `/dev/ttyUSB0`:

**Step 1: Ensure auto-dew is disabled on the hardware**

Auto-dew must be OFF before running ConformU, otherwise the dew heater write tests will fail (CanWrite will return false for switches 2 and 3).

**Step 2: Start the ppba-switch service**

```bash
# Start the service with the real hardware configuration
cargo run -p ppba-switch -- -c services/ppba-switch/config.json
```

The service will connect to the PPBA on `/dev/ttyUSB0` and start the Alpaca server on port 11112.

**Step 3: Run ConformU**

In a separate terminal, run ConformU with default hardware timing (recommended for real hardware):

```bash
# Run ConformU against the Switch device with default timing
conformu conformance http://localhost:11112/api/v1/switch/0
```

**Note:** We use ConformU's default timing settings for real hardware tests (SwitchReadDelay: 500ms, SwitchWriteDelay: 3000ms). These conservative delays ensure reliable operation with actual hardware. The automated CI tests use reduced delays with mock hardware for faster execution.

**Expected results:**
- All tests should pass with 0 errors and 0 issues
- Test duration: ~10 minutes with default timing
- ConformU will test all 16 switches including read/write operations on controllable switches

**Troubleshooting:**
- If dew heater write tests fail with "Expected P3:XXX, got: P3:0" or "Expected P4:XXX, got: P4:0", auto-dew is enabled. Disable it before running ConformU.
- If the service fails to start, ensure no other process is using port 11112 or `/dev/ttyUSB0`
- If connection fails, verify the PPBA is powered on and connected via USB

## Dependencies

- `ascom-alpaca` - ASCOM Alpaca server and device traits
- `tokio-serial` - Async serial port communication
- `tokio` - Async runtime
- `serde` / `serde_json` - Configuration parsing
- `rusty-photon-config` - Config-path resolution + first-run `UniqueID` materialization
- `clap` - Command-line argument parsing
- `tracing` - Logging
- `thiserror` - Error handling

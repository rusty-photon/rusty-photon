# PPBA Switch Driver

ASCOM Alpaca Switch driver for the Pegasus Astro Pocket Powerbox Advance Gen2 (PPBA).

## Overview

This service exposes the PPBA device as an ASCOM Alpaca Switch device, allowing control of power outputs, dew heaters, and monitoring of device sensors through the standard ASCOM Switch interface.

## Hardware identity

| Property | Value |
|----------|-------|
| USB | FTDI `0403:6015` |
| USB product descriptor | `PPBADV Gen2C` |
| `usb_model` in `pkg/doctor.toml` | `PPBA` |

`0403:6015` is FTDI's generic bridge chip, shared with other Pegasus Astro
serial devices on the fleet, so the VID:PID identifies the *family* and not
the unit. What separates them is the product descriptor the box publishes on
the bus, and `usb_model` is matched as a substring of it.

Transcribe that value from a bus — sysfs `product` on Linux,
`DEVPKEY_Device_BusReportedDeviceDesc` on Windows — never from the protocol
table below. `PPBA_OK` is the box's answer to `P#` over the serial link: the
driver reads it as payload on an open port, and it is never published as the
product string this box declares while enumerating. A `usb_model` written
from it would match no descriptor at all, and `rusty-photon-doctor`'s only
way to report that is to call a present device unplugged.
[doctor](doctor.md#the-derived-catalog) carries
the descriptor table for every service that declares a model.

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

### `PA` response layout

Captured from the dev box's unit (`PPBADV Gen2C`, firmware `2.12.3`) rather
than copied from the vendor table:

```
PPBA:12.0:14:27.4:68:21.0:0:1:52:52:1:0:3
```

| # | Field | Notes |
|---|-------|-------|
| 0 | prefix | `PPBA:` |
| 1 | voltage | Volts, decimal |
| 2 | current | raw sense count, integer 0-1024; **÷ 65** for Amps |
| 3 | temperature | °C, decimal |
| 4 | humidity | % RH, integer |
| 5 | dewpoint | °C, decimal |
| 6 | quad | `0`/`1` |
| 7 | adj | `0`/`1` |
| 8-9 | dewA / dewB | PWM duty, 0-255 |
| 10 | autodew | `0`/`1` |
| 11 | warn | `0`/`1` |
| 12 | pwradj | adjustable-output setting |

The divisor lives in the parser, so the switch table and the ASCOM surface
only ever see Amps. The count is an unsigned integer on the wire, so a
decimal in that slot fails the parse like any other corrupted field rather
than being read as a value already in Amps.

### The current is a sense count, not Amps

`PA`'s current field is the one field of that reply not already in its
unit. Pegasus's command table (firmware 2.5 and later) calls it a *"sens
current 0-1024"* that is converted to Amps by dividing by 65, a scale
the PPBA keeps for compatibility with the original Pocket Powerbox. INDI's
PPBA driver applies the same divisor.

Publishing the count as Amps is not only a wrong number: a count of 40 was
published as 40 A from a box drawing about 0.6 A, far outside the 0-20 A
the switch declares for itself, which is an ASCOM conformance failure as
well as a misreading. Scaled by 65, the top of the documented sense range,
1024, is 15.75 A, so every count the device documents sending lands inside
the published range. The parser does not clamp or range-check the count,
the same as every other `PA` field: a count above 1024 is outside the
device's contract, and is published as read rather than hidden.

The divisor was checked against the box's own amp-hour counter, because a
fixture built from the vendor table only confirms the table. Two Gen2C
units were measured, each at a steady load, over a delta of `PS`'s
amp-hour field (resolution 0.01 Ah):

| Unit | `PA` count | Amp-hours | Counts per Amp | `÷ 100` would predict |
|------|-----------|-----------|----------------|------------------------|
| dev box, quad off | 14 | +0.10 over 1764 s | 62.4-76.2 | +0.069 Ah |
| pier1, quad on | 41.1 (mean) | +0.16 over 897 s | 60.2-68.2 | +0.102 Ah |

Both admit 65 and rule out a centi-amp reading, and two loads a factor of
three apart through the same scale are what a linear, zero-offset count
predicts. Together they bound the divisor to 62.4-68.2.

**`PS`'s watt-hour field is not an energy integral, so it cannot calibrate
anything.** It *falls* under a rising load: on pier1 it read 296.03 Wh,
then 295.57 Wh, as the count climbed from 38 to 42 and the input sagged
from 12.0 to 11.9 V, and it fell in 55 of 180 five-second intervals. It
tracks cumulative amp-hours times the present input voltage (Wh / Ah went
11.956 → 11.925 over the run), so a delta of it folds in any voltage
drift. Use the amp-hour field.

**On a Gen2C the count is the total draw, not the Quad 12V group.** The
vendor table labels this field the Quad 12V outputs' current. Over the dev
box's run the quad output was off, and `PC` (*Print Power Metrics*, whose
currents are already in Amps) reported `total_current` 0.2 A and
`current_12V_outputs` 0.0 A while `PA` sent 14 counts, 0.215 A. The field
follows the total, which is why switch 11 is Total Current. The driver
still reads it from `PA` rather than `PC`: `PC` reports to 0.1 A, while one
count of the sense field is about 0.015 A.

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
| 8 | Watt Hours | Wh | 0 | 99999 | 0.1 | `PS` command, as the device reports it: amp-hours × present voltage, not an energy integral (see [the sense count](#the-current-is-a-sense-count-not-amps)) |
| 9 | Uptime | Hours | 0 | 99999 | 0.01 | `PS` command |

### Read-Only Switches - Sensor Data (CanWrite = false)

| ID | Name | Type | Min | Max | Step | Source |
|----|------|------|-----|-----|------|--------|
| 10 | Input Voltage | Volts | 0 | 15 | 0.1 | `PA` command |
| 11 | Total Current | Amps | 0 | 20 | 0.01 | `PA[2] ÷ 65` (see [the sense count](#the-current-is-a-sense-count-not-amps)) |
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
| observingconditions | averaging_period | Sliding-window length for sensor means (humantime); `0` selects the instantaneous window, `24h` is the ceiling (see [`AveragePeriod`](#averageperiod-staleness-and-the-meaning-of-zero)) | `"5m"` |

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

### `AveragePeriod`, staleness, and the meaning of zero

The three sensor means are windowed on **read**, not only on insert. Samples
are evicted when a new one arrives, so a session whose poll loop is failing
while it stays open holds a buffer of readings that all aged out of the window
with nothing arriving to replace them. Averaging those and answering with them
would report an hours-old dewpoint as current, which is what a client decides
dew-heater duty from. A window holding only aged-out samples therefore reads as
`VALUE_NOT_SET` — the same code the device returns before the first poll — for
`Temperature`, `Humidity` and `DewPoint`.

ASCOM reads `AveragePeriod = 0` as "the device is not averaging — give me the
most recent value". The means have no unaveraged mode, and because the window
is applied on read, a literal zero-length window would answer `VALUE_NOT_SET`
at every read. An unbounded window is no better: it would report an hours-old
sample from a stalled poll loop as current.

Zero therefore maps to the shortest window that still always holds the newest
sample under healthy polling: `max(3 × serial.polling_interval, 10s)`. It is
measured in poll intervals rather than seconds because the cadence is
configurable — a fixed 10 s window against a 60 s cadence would leave the
sensors reading `VALUE_NOT_SET` for 50 seconds out of every 60. Three
intervals tolerates two missed polls before readings degrade, and the 10 s
floor keeps a fast cadence from making the window shorter than one client
round trip. Config seeding and `SetAveragePeriod` share that one mapping, so a
period written to the config file behaves exactly like the same period set
over the wire.

The period a client sets is stored verbatim and read back as-is, rather than
inferred from the resulting window. Inferring it cannot represent zero (the
window is never zero) and makes a genuine average whose length happens to
equal the instantaneous window indistinguishable from "not averaging".

`SetAveragePeriod` accepts a finite value in `[0, 24]` hours and rejects
everything else with `INVALID_VALUE` — including `NaN`, which an Alpaca client
can send because `f64::from_str` parses `"NaN"`. The check is one range test
rather than a pair of ordered comparisons on purpose: every ordered comparison
against `NaN` is false, so `NaN` would otherwise pass validation and reach the
seconds-to-`Duration` conversion, which panics on a non-finite value
([#1247](https://github.com/rusty-photon/rusty-photon/issues/1247)).

The manager holds the same line for callers that do not come through the
device: `set_averaging_period` leaves every sensor window *and* the recorded
period untouched when handed a period outside `[0, 24]` hours. Half-applying
such a period would be worse than rejecting it — the windows would fall back
while `AveragePeriod` read back the value they never used. The
seconds-to-`Duration` conversion underneath is fallible too, so no path
through this code can panic.

All three checks — the device, the manager, and the `config.apply` bound
below — read the ceiling from one `MAX_AVERAGING_PERIOD` constant in
`manager.rs`, so the runtime range and the persisted range cannot drift
apart.

`config.apply` validates `averaging_period` against the same bounds the device
enforces on `SetAveragePeriod`: no lower bound (zero is meaningful), and a 24
hour ceiling, which is ASCOM's. The two must agree — a period a client can
select at runtime but not persist, or persist but not select, is a trap either
way.

The rolling-mean implementation is shared with `upbv2-driver` and lives in
[`rusty-photon-rolling-stats`](../crates/rusty-photon-rolling-stats.md).

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
cargo run -p ppba-driver -- -c config.json

# With command-line overrides
cargo run -p ppba-driver -- --port /dev/ttyUSB1 --server-port 11113

# With debug logging
cargo run -p ppba-driver -- -c config.json -l debug
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
│   ├── serial.rs                     # PpbaTransportFactory (open_serial_port → SerialFrameTransport)
│   ├── mock.rs                       # MockPpbaTransportFactory (feature-gated)
│   └── switches.rs                   # Switch definitions
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

1. **Shared transport via `rusty-photon-shared-transport`**: refcounted lifecycle, command-lock arbitration, while-open poll task, and the connect/handshake/teardown sequence all live in the shared crate. `PpbaCodec` plugs in the `P#`/`PA`/`PS` framing and response parsing; `PpbaTransportFactory` opens the port through the shared crate's `open_serial_port` — one opener for every serial driver, carrying the builder settings, the error mapping, and the bounded retry that rides out a Windows handle still closing — and wraps the stream in a `SerialFrameTransport`. Each ASCOM device holds `Option<Session<PpbaCodec>>` — the session existing is the canonical "Connected" state, so the previously-separate "requested" flag can't desync from the underlying transport (issue #251 cannot reoccur).

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
- `SetSwitch` and `SetSwitchValue` on switches 2 or 3 return `NOT_IMPLEMENTED`. The driver sends the `PA` status query that reads auto-dew, and no heater command (`P3`/`P4`)
- Error message: "cannot write to switch X while auto-dew is enabled. Disable auto-dew (switch 5) first."

**When auto-dew is DISABLED (switch 5 = OFF):**
- `CanWrite(2)` and `CanWrite(3)` return `true` (writable)
- Switches 2 & 3 can be written normally

**When disconnected:**
- `CanWrite()` for any switch returns a `NOT_CONNECTED` error (per ASCOM specification)

**Why `NOT_IMPLEMENTED`, not `INVALID_OPERATION`.** "The heater is busy
with auto-dew" reads like an operation error, but ASCOM ties the Switch
write methods to `CanWrite`: `SetSwitch` and `SetSwitchValue` must raise
`MethodNotImplemented` when `CanWrite` is false for that switch, and
ConformU judges every write by the `CanWrite` it read for the switch. A
heater under auto-dew already reports `CanWrite = false`, so any other code
makes the two answers disagree, which on hardware was four ConformU issues
(one per write method per heater). `upbv2-driver` classifies its auto-dew
refusal the same way. The message still tells the operator what to do.

The auto-dew check comes before the range check, so an out-of-range value
on a heater under auto-dew is also answered `NOT_IMPLEMENTED`: the switch
cannot be written, whatever the value.

### State Caching and Refresh Behavior

The driver caches device state to minimize serial communication overhead:

- **Background polling**: Device state is refreshed every `polling_interval` (default: `"5s"`)
- **CanWrite() queries**: Use cached state (may be up to `polling_interval` stale if auto-dew changed externally), except for dew heaters (switches 2 & 3) which refresh the cache if not yet populated to ensure accurate writability reporting
- **SetSwitch() / SetSwitchValue() for dew heaters**: Refreshes state (`PA`) immediately before the auto-dew check (always validates against current device state)
- **After successful writes**: State is refreshed immediately to reflect the change
- **External changes**: Auto-dew changes made by other clients or via serial are detected within the polling interval

For tighter synchronization with external changes, reduce `polling_interval` in the configuration. However, note that very short intervals (< 1s) increase serial communication overhead.

**The check and the write are separate serial requests.** The transport is
locked per request, not across a heater write's refresh, check and send, so
another client's `PD:1` can land between the driver's `PA` and its `P3`/`P4`.
The heater write then reaches a box that has just turned auto-dew on. The
driver leaves this window open because the box makes it harmless. Measured on
the dev box's PPBADV Gen2C (fw 2.12.3) with auto-dew on:
- the box accepts a manual `P3:0` (it echoes `P3:0`, and `PA` reads heater A
  at 0);
- auto-dew stays on;
- its own loop puts heater A back to its computed 52 within 6 s.

So the stray value lasts at most one auto-dew cycle. It never turns auto-dew
off.

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
# This will fail with NOT_IMPLEMENTED (0x400):
curl -X PUT http://localhost:11112/api/v1/switch/0/setswitchvalue \
  -d "Id=2&Value=128"
# Error: "cannot write to switch 2 while auto-dew is enabled. Disable auto-dew (switch 5) first."
```

### Client Recommendations

For robust client applications:

1. **Always connect first**: `CanWrite()` requires an active connection
2. **Check CanWrite() before writing**: Query `CanWrite(id)` to determine if a switch is currently writable
3. **Handle write errors gracefully**: Catch `NOT_IMPLEMENTED` when writing to dew heaters. From a heater it means "not writable right now", since auto-dew can be switched on between your `CanWrite` and your write; re-read `CanWrite` rather than treating the heater as permanently read-only
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

ConformU passes in either auto-dew state, and the two states test different
things:
- **Auto-dew off:** ConformU finds both heaters writable and drives them
  across 0-255.
- **Auto-dew on** (the box's normal state): it finds them read-only and
  checks that `SetSwitch` and `SetSwitchValue` answer `NOT_IMPLEMENTED`.

ConformU tests switches in ascending order and reads each one's `CanWrite`
once, before that switch's write tests. So switches 2 and 3 are judged before
it reaches switch 5. At switch 5 it toggles auto-dew and then writes back the
value it found. The in-tree ConformU test runs the Switch device in both
states (see [ConformU Compliance Testing](#conformu-compliance-testing)).

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

### The mock's pinned frame

`src/mock.rs` serves fixed frames, and `sensor_readings.feature` asserts the
current exactly, so a scaling regression has to fail there:

```
PA  PPBA:12.5:130:25:60:15.5:1:0:128:64:0:0:0
PS  PS:2.5:10.5:126:3600000
```

The current slot carries an integer sense count, as the hardware sends it,
so switch 11 reads 2.0 A (130 / 65). A fixture that emitted a decimal such
as `3.2` there would describe a frame no PPBA produces, and would agree with
any parser written from the same misreading of the vendor table. The same
feature also asserts that every switch reads inside the range it publishes,
the check ConformU applies, so a scaling error surfaces as a failing
scenario rather than as an out-of-range reading a client has to notice.

### ConformU Compliance Testing

The driver includes ASCOM ConformU compliance tests that verify conformance to the ASCOM Switch and ObservingConditions interface specifications. These tests run in CI via the `conformu.yml` workflow.

**Performance optimization**: ConformU uses configurable delays between Switch read/write operations. The test passes a `bdd_infra::FullRunSettings` with reduced delays:
- `SwitchReadDelay`: 50ms (default: 500ms)
- `SwitchWriteDelay`: 100ms (default: 3000ms)

This reduces a Switch pass from ~8 minutes to ~35 seconds per platform.

**Two Switch passes, one per auto-dew state.** The mock starts with auto-dew
off, so the first pass drives both dew heaters through ConformU's write
tests. A second Switch pass, against a fresh mock, first turns auto-dew on
over Alpaca (`SetSwitchValue(5, 1)`, as a client would), then runs ConformU
with switches 2 and 3 read-only. It is the pass that holds the driver to
`NOT_IMPLEMENTED` for a refused heater write. No mock knob is needed, unlike
`upbv2-driver`'s `UPBV2_MOCK_AUTO_DEW`: this driver writes `PD` itself, and
the mock honours it.

**What the settings can and cannot do**: `FullRunSettings` carries only timeouts and delays. ConformU's URL-argument verbs (which `bdd_infra::run_conformu` drives) call `SetFullTest()` after reading the settings file, so every test-selection setting is force-enabled — `SwitchEnableSet` included, which means the mock run **does** exercise the Switch write tests. A ConformU settings file needs only `SettingsCompatibilityVersion`; every property it omits keeps ConformU's default.

#### Running ConformU Against Real Hardware

To run ConformU compliance tests against the actual PPBA hardware on `/dev/ttyUSB0`:

**Step 1: Note the auto-dew state**

Run in whichever auto-dew state the box is in; both pass (see [ConformU Testing](#conformu-testing)). Note the state first. ConformU writes every writable switch, auto-dew included, and puts each back to the value it found. Check auto-dew afterwards anyway: ConformU's `SetSwitch` restore always writes `false`, and only the later `SetSwitchValue` restore puts the original back.

**Step 2: Start the ppba-driver service**

```bash
# Start the service with the real hardware configuration
cargo run -p ppba-driver -- -c services/ppba-driver/config.json
```

The service will connect to the PPBA on `/dev/ttyUSB0` and start the Alpaca server on port 11112.

**Step 3: Run ConformU**

In a separate terminal, run both suites against both devices with default hardware timing (recommended for real hardware). A run that is to be recorded follows [hardware-validation.md](../skills/hardware-validation.md), version gate first:

```bash
conformu alpacaprotocol http://localhost:11112/api/v1/switch/0              -n alpacaprotocol-switch.log
conformu conformance    http://localhost:11112/api/v1/switch/0              -n conformance-switch.log -r conformance-switch-results.json
conformu alpacaprotocol http://localhost:11112/api/v1/observingconditions/0 -n alpacaprotocol-observingconditions.log
conformu conformance    http://localhost:11112/api/v1/observingconditions/0 -n conformance-observingconditions.log -r conformance-observingconditions-results.json
```

**Note:** We use ConformU's default timing settings for real hardware tests (SwitchReadDelay: 500ms, SwitchWriteDelay: 3000ms). These conservative delays ensure reliable operation with actual hardware. The automated CI tests use reduced delays with mock hardware for faster execution.

**Expected results:**
- All four suites pass with 0 errors and 0 issues, in either auto-dew state
- Test duration with default timing, for the Switch `conformance` suite: about 9 minutes with auto-dew off and about 3 with it on. The heaters' write tests account for the difference: with auto-dew on those writes are refused immediately and never reach ConformU's 3000 ms write delay. The other suites take seconds.
- ConformU tests all 16 switches, with read/write tests on every switch that reports `CanWrite = true` (both heaters only when auto-dew is off)

**Troubleshooting:**
- If the service fails to start, ensure no other process is using port 11112 or `/dev/ttyUSB0`
- If connection fails, verify the PPBA is powered on and connected via USB

## Real-hardware validation

The evidence trail is [`docs/validation/`](../validation/README.md);
this service's runs, newest first:

- **2026-09-27 — PPBADV Gen2C on Linux x86_64, auto-dew on**
  ([record](../validation/2026-09-27-ppba-driver-ppba-gen2c-linux-auto-dew-on/README.md)).
  Both devices, both suites, clean in the box's normal state. ConformU finds
  the dew heaters read-only and confirms that both write methods answer
  `NOT_IMPLEMENTED`, the four checks that were issues before this was fixed.

- **2026-09-27 — PPBADV Gen2C on Linux x86_64**
  ([record](../validation/2026-09-27-ppba-driver-ppba-gen2c-linux/README.md)).
  The first hardware record: both devices, both suites, clean, with auto-dew
  off. It puts the Total Current scaling on hardware and exercises every
  writable switch, including both dew heaters across 0-255.

## Dependencies

- `ascom-alpaca` - ASCOM Alpaca server and device traits
- `rusty-photon-shared-transport` - Refcounted transport lifecycle and `open_serial_port`
- `tokio` - Async runtime
- `serde` / `serde_json` - Configuration parsing
- `rusty-photon-config` - Config-path resolution + first-run `UniqueID` materialization
- `clap` - Command-line argument parsing
- `tracing` - Logging
- `thiserror` - Error handling

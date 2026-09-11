# UPBv2 Driver

ASCOM Alpaca Switch and ObservingConditions driver for the Pegasus Astro
Ultimate Powerbox v2 (UPBv2).

## Overview

The UPBv2 is a 12 V power and USB distribution box with environmental
sensors and an onboard stepper driver. This service exposes it as an ASCOM
Alpaca **Switch** device (four 12 V outputs, three dew channels, six USB
ports, one variable-voltage output, plus per-channel current and
overcurrent telemetry) and an **ObservingConditions** device (temperature,
humidity, dewpoint).

It is a **separate service from [`ppba-driver`](ppba-driver.md)**, not a
second mode of it. The two devices share a vendor and a serial framing but
not a command language: `P3:`/`P4:` set *dew heaters* on the PPBA and *12 V
output ports* on the UPBv2, and `PS` means *power statistics* on the PPBA
but *boot power state* on the UPBv2 (statistics moved to `PC`). A shared
command enum would let a mis-identified unit energise a 12 V rail while the
driver believed it was setting a heater duty cycle — a tenet 2 hazard. The
reuse that matters is already factored out at crate level
(`rusty-photon-shared-transport`, `-server-config`, `-config`, `-driver`,
`-service-lifecycle`, `-tls`, `rp-auth`, `-i18n`, `-doctor-checks`).

A second, independent reason the models cannot share one binary: the
exposed ASCOM device set is fixed when `ServerBuilder::build()` registers
devices from `config.<device>.enabled`, *before* any handshake. A combined
binary could not detect the model and register the right device set; it
would need an operator-declared `"model"` key, at which point one binary
buys nothing over two.

## Device identity

| Property | Value |
|----------|-------|
| USB | FTDI `0403:6015` |
| USB product descriptor | `UPBv2 revA` |
| Serial settings | 9600 baud, 8N1, `\n`-terminated |
| Firmware baseline | >= 2.4 (Feb 2021) |

`0403:6015` is FTDI's generic bridge chip, shared with other Pegasus Astro
serial devices on the fleet, so VID:PID identifies the *family*, and the
configured port path (or the `by-id` / FTDI serial string) identifies the
*unit*. `pkg/doctor.toml` carries `usb_model = "UPBv2"` on that
understanding: doctor matches it as a substring of the descriptor this box
publishes on the bus (sysfs `product` on Linux,
`DEVPKEY_Device_BusReportedDeviceDesc` on Windows).
[doctor](doctor.md#the-derived-catalog) carries the descriptor table for
every service that declares a model; each driver document states its own.

The descriptor is **not** the handshake reply. `P#` answers `UPB2_OK` over
the serial link, and nothing carries that string to the USB host, which is
told `UPBv2 revA`. A `usb_model` copied from the protocol table rather than
read off a bus is a value no device can ever match, and doctor's only way to
say so is to report a plugged-in, powered, actively driven box as missing.

## Device Protocol

### Commands the driver uses

| Command | Description | Response |
|---------|-------------|----------|
| `P#` | Ping / status check | `UPB2_OK` |
| `PV` | Firmware version | `n.n` |
| `PA` | Full status and sensor readings | 21 colon-separated tokens (below) |
| `PC` | Power consumption counters | `avgAmps:ampHours:wattHours:uptime_ms` |
| `PS` | Boot power state + variable-voltage setting | `PS:bbbb:nn` documented; `PS:110:6` on the wire — see below |
| `P1:b` … `P4:b` | 12 V output 1-4 on/off | `Pn:b` |
| `P5:nnn` … `P7:nnn` | Dew channel A/B/C PWM duty, 0-255 | `Pn:nnn` |
| `P8:nn` | Variable output voltage, 3-12 V | `P8:nn` |
| `U1:b` … `U6:b` | USB port 1-6 on/off (1-4 USB3, 5-6 USB2) | `Un:b` |

`PS` is read for one field only — the variable-voltage setpoint, which `PA`
does not report. Its boot-state field is parsed and discarded.

### Commands the driver deliberately does not send

These stay with the Pegasus Astro desktop software for now. Excluding them
is a scope decision, not an oversight; each has a reason to stay out.

| Command | Why excluded |
|---------|--------------|
| `PE:bbbb` | Sets the power-on-boot state. Out of scope. |
| `US:bbbbbb` | Sets the USB-on-boot state. Out of scope. |
| `PD:b` | Sets auto-dew. Out of scope as a *write*; the state is still **read** from `PA` (see [Auto-dew interaction](#auto-dew-interaction)). |
| `DA` | Reports auto-dew aggressiveness — only meaningful alongside the `PD:` write path. |
| `PL:b` | LED indicator. The vendor command table warns that on PCB revision C and later the LED signal pin doubles as the stepper driver's sleep line: **`PL:0` puts the motor to sleep.** Never sent. |
| `PZ:b` | Master on/off for all four outputs plus dew heaters. A single ASCOM switch whose write silently changes seven other switches' states is a non-orthogonal table; the individual switches cover the same ground. *Open item — see below.* |
| `PF` | Reboots the device. |
| `PI`, `PR` | I²C reset and connected-device list. `PR` is diagnostically interesting (it names the environmental sensor as `DHT` or `HDC` and reports whether an external motor controller is present) but is not switch state. Candidate for a handshake-time `debug!` only. |
| `SC:`, `SS:`, `SR:`, `SB:`, `SJ:` | Stepper configuration (sync position, max speed, reverse, backlash, acceleration). All EEPROM-stored, none maps to a standard ASCOM Focuser property. Same rationale: Pegasus software owns them. |
| `XS:*` | External motor controller pass-through. No such hardware in the fleet. |

### `PA` response layout

Captured from rig2's unit rather than copied from the vendor table:

```
UPB2:12.7:0.0:0:40.9:32:20.9:0010:110001:0:0:0:0:0:0:0:0:0:0:0000000:1
```

| # | Field | Notes |
|---|-------|-------|
| 0 | prefix | `UPB2:` on the wire. The vendor table's example line says `UPB:` and its own field legend says `UPB2:`; the parser accepts both. |
| 1 | voltage | Volts, decimal |
| 2 | current | Amps, decimal — total draw |
| 3 | power | Watts, integer |
| 4 | temperature | °C, decimal |
| 5 | humidity | % RH, decimal |
| 6 | dewpoint | °C, decimal |
| 7 | port status | 4 chars, one per 12 V output |
| 8 | usb status | 6 chars, one per USB port |
| 9-11 | dew1-3 duty | 0-255 |
| 12-15 | port 1-4 current | raw; **÷ 480** for Amps |
| 16-17 | dew A/B current | raw; **÷ 480** for Amps |
| 18 | dew C current | raw; **÷ 700** for Amps (different MOSFET) |
| 19 | overcurrent | 7 chars: outputs 1-4 then dew A-C; `1` = tripped |
| 20 | auto-dew mask | 0-7, see below |

The scaling divisors live in the parser, so the switch table and the ASCOM
surface only ever see Amps.

Because `PA` reports the USB port states directly, this driver needs **no
shadow state** for them — unlike `ppba-driver`, whose `PA` omits the USB hub
and which therefore tracks it locally after every write.

## ASCOM device set

| Device | Status |
|--------|--------|
| Switch | MVP |
| ObservingConditions | MVP |
| Focuser | **Deferred** — see below |

Bundling Switch and ObservingConditions into one service is correct under
[ADR-014](../decisions/014-zwo-per-device-services-and-link-features.md)'s
"one service per independently usable device, bundled only when the hardware
forces it": there is one serial port and one command stream, and the sensors
are reachable only through the powerbox.

### Why the Focuser is deferred

The UPBv2 has an onboard stepper driver (`SA`, `SP`, `SM:`, `SG:`, `SH`,
`SI`, `ST`) that maps cleanly onto an absolute ASCOM Focuser with a
temperature probe and no onboard temperature compensation. It is deferred
from the MVP for one reason: **there is no motor on the UPBv2 in the fleet
to validate against.** rig2's focuser is an Optec FocusLynx on its own
serial port, exposed through the Optec Alpaca driver. Shipping a focuser
whose only evidence is a mock repeats the mistake recorded against the QHY
connect path — never ship a device path on mock evidence alone.

The protocol is documented above so the phase is cheap to pick up when a
Pegasus stepper is actually attached. When it lands it belongs in *this*
service (same serial port, same command stream), config-gated
`focuser.enabled` defaulting to `false`, with `SA` joining the poll loop
only when enabled.

## Switch mapping

**MaxSwitch = 39.** Ids are contiguous from zero and stable; the enum
variant order is not the id (`SwitchId::info().id` is), matching
`ppba-driver`'s convention.

### Writable (CanWrite = true)

| Id | Name | Command | Min | Max | Step |
|----|------|---------|-----|-----|------|
| 0-3 | 12V Output 1-4 | `P1:b`-`P4:b` | 0 | 1 | 1 |
| 4-6 | Dew Heater A/B/C | `P5:nnn`-`P7:nnn` | 0 | 255 | 1 |
| 7 | Variable Output Voltage | `P8:nn` | 3 | 12 | 1 |
| 8-13 | USB Port 1-6 | `U1:b`-`U6:b` | 0 | 1 | 1 |

Switch 7 is the one write that persists to EEPROM. It is never written
speculatively — only on an explicit `SetSwitchValue`.

Dew heaters 4-6 are dynamically read-only while their channel is under
auto-dew control; see below.

### Read-only (CanWrite = false)

| Id | Name | Source | Min | Max | Step |
|----|------|--------|-----|-----|------|
| 14 | Input Voltage | `PA[1]` | 0 | 15 | 0.1 |
| 15 | Total Current | `PA[2]` | 0 | 25 | 0.01 |
| 16 | Power Draw | `PA[3]` | 0 | 300 | 1 |
| 17 | Temperature | `PA[4]` | -40 | 60 | 0.1 |
| 18 | Humidity | `PA[5]` | 0 | 100 | 1 |
| 19 | Dewpoint | `PA[6]` | -40 | 60 | 0.1 |
| 20-23 | Output 1-4 Current | `PA[12..15] / 480` | 0 | 10 | 0.01 |
| 24-25 | Dew A/B Current | `PA[16..17] / 480` | 0 | 5 | 0.01 |
| 26 | Dew C Current | `PA[18] / 700` | 0 | 5 | 0.01 |
| 27-30 | Output 1-4 Overcurrent | `PA[19]` chars 0-3 | 0 | 1 | 1 |
| 31-33 | Dew A/B/C Overcurrent | `PA[19]` chars 4-6 | 0 | 1 | 1 |
| 34 | Auto-Dew Channels | `PA[20]` | 0 | 7 | 1 |
| 35 | Average Current | `PC[0]` | 0 | 25 | 0.01 |
| 36 | Amp Hours | `PC[1]` | 0 | 9999 | 0.01 |
| 37 | Watt Hours | `PC[2]` | 0 | 99999 | 0.1 |
| 38 | Uptime | `PC[3]`, ms → hours | 0 | 99999 | 0.01 |

The overcurrent flags are exposed individually rather than as one aggregate
warning: when a rail trips at 2 a.m. the useful fact is *which* one. The
device shuts the affected port down on its own when it trips.

### Operator labels

A port number is what the box knows; what is plugged into it is what the
operator knows. `switch.labels` carries that fact across: it replaces the
built-in name of any switch that corresponds to a connector. An absent or
empty block leaves every name exactly as the tables above list them.

```json
"switch": {
  "name": "Pegasus UPBv2 Switch",
  "labels": {
    "12V Output 1": "QHY600",
    "12V Output 2": "Flat Panel",
    "12V Output 4": "Focuser",
    "USB Port 5": "COM3 Focuser"
  }
}
```

Two things to know about the shape:

- **Keys are built-in names, not ids.** `"12V Output 1"`, not `"0"`. The file
  is then readable without the id table open, and a key naming no labellable
  switch is rejected — which is what makes a typo loud instead of silently
  inert.
- **A label follows its port's telemetry.** Labelling `12V Output 1` as
  `QHY600` also renames that port's current reading (id 20) and its
  overcurrent flag (id 27). The switch table already argues that the useful
  fact when a rail trips at 2 a.m. is *which* rail; a port number is not that
  fact, and a label that stopped at id 0 would leave the alarm row still
  speaking in port numbers.

Three rules, all enforced when the config is **deserialized** rather than by a
separate validation pass, so a bad map fails at startup — and fails a
`config.apply` — with the offending entry named:

1. **Only ids 0-13 may be labelled**: the four 12 V outputs, the three dew
   channels, the variable output and the six USB ports. Those are the
   switches an operator plugs equipment into. The read-only rows are
   physical quantities and keep their names — a client that saw `Humidity`
   renamed would have no way to know what it was reading.
2. **A label needs non-whitespace content.** Removing the entry is how a
   switch goes back to its built-in name. `""` or a run of spaces is
   *rejected* rather than quietly meaning the same thing, so a half-finished
   edit fails loudly instead of passing for a deliberate reset.
3. **The 39 names stay unique.** ASCOM clients key on the name, so a label
   that collides — with another label, or with the built-in name of a switch
   left unlabelled — is rejected.

`GetSwitchDescription` is untouched. The description already names the
physical port, so the port stays identifiable after the name is replaced:

| Id | `GetSwitchName` | `GetSwitchDescription` |
|----|-----------------|------------------------|
| 0 | `QHY600` | Switches the 12V output on port 1 |
| 20 | `QHY600 Current` | Current draw of this 12V output in Amps |
| 27 | `QHY600 Overcurrent` | Overcurrent or short-circuit flag for this 12V output. The device shuts the port down when it trips |

Labels are an ordinary config field, so `config.apply` edits them and the
service reloads onto the new names. `SetSwitchName` stays `NOT_IMPLEMENTED`:
a name written over the wire would not survive a restart, and the config file
is the one place the mapping is recorded.

Pegasus Unity keeps the operator's own labels in its private SQLite database
(`%APPDATA%\PegasusAstroUnityPlatform\Production\Server\DB.sqlite`, table
`KeyValueStorage`, keys `PowerHubControl.btne_<n>` for the 12 V outputs and
`USBHubControl.btne_<n>` for the USB ports). This driver deliberately does
not read it: another vendor's private storage, Windows-only, and a schema
this project does not control. Copy the labels across once.

The label map itself is `SwitchLabels` from
`crates/rusty-photon-server-config`, shared with
[`ppba-driver`](ppba-driver.md#operator-labels) — the two configs stay
parallel because they are the same type, parameterised by each driver's own
switch table.

## Auto-dew interaction

Auto-dew is **readable but not settable** by this driver. `PA[20]` carries a
channel mask the driver reads every poll; `PD:` is never sent.

| Mask | Channels under auto-dew control |
|------|--------------------------------|
| 0 | none |
| 1 | A, B, C |
| 2 | A |
| 3 | B |
| 4 | C |
| 5 | A, B |
| 6 | A, C |
| 7 | B, C |

`CanWrite` for dew switches 4-6 is computed **per channel** from that mask:
a channel the device is driving reports `CanWrite = false`, and a write to
it fails `NOT_IMPLEMENTED` with a message naming the Pegasus software as
the place to turn auto-dew off. `NOT_IMPLEMENTED` rather than the more
descriptive `INVALID_OPERATION` because ASCOM couples the two: a switch
reporting `CanWrite = false` must raise `MethodNotImplemented` from
`SetSwitch` / `SetSwitchValue`, and ConformU checks that pairing. This is
strictly better than
`ppba-driver`'s all-or-nothing gate, and it costs nothing — the field is in
a reply the driver already parses.

The same value is surfaced as read-only switch 34 so a client can *explain*
a false `CanWrite` rather than just observe it.

**ConformU consequence:** as with the PPBA, a compliance run against real
hardware needs auto-dew set to `0` beforehand, or the dew-heater write tests
fail on a read-only switch. That is now done in the Pegasus software rather
than through the driver.

## Connect, handshake and polling

Tenet 3 (**no actuation on connect**) governs this device more directly than
most: nearly every write it accepts is a power toggle, which the tenet names
explicitly. Therefore:

- The handshake is **read-only**: `P#` → `PV` → `PA` → `PC` → `PS`. It
  seeds the cache and validates the unit is a UPBv2 (`UPB2_OK`). It re-runs
  on every reconnect after a serial glitch, so it must stay read-only by
  construction.
- The poll loop refreshes `PA` + `PC` + `PS` every `polling_interval`
  (default 5 s) into the shared cache. Reads are served from cache; a write
  refreshes on demand.
- `config.apply` never pushes output states to hardware.
- The excluded `PE:`/`US:` boot-state commands would have been the one place
  a config-driven output state could leak onto the hardware. Their exclusion
  removes that path entirely.

A UPBv2 that is powered but has all outputs off is a normal, connectable
state — the driver reports it and changes nothing.

## Configuration

```json
{
  "serial": {
    "port": "COM5",
    "baud_rate": 9600,
    "polling_interval": "5s",
    "timeout": "2s"
  },
  "server": { "port": 11127, "bind_address": "0.0.0.0", "tls": null, "auth": null },
  "switch": {
    "name": "Pegasus UPBv2 Switch",
    "unique_id": "",
    "description": "Pegasus Astro Ultimate Powerbox v2 Power Control",
    "enabled": true,
    "labels": { "12V Output 1": "QHY600" }
  },
  "observingconditions": {
    "name": "Pegasus UPBv2 Weather",
    "unique_id": "",
    "description": "Pegasus Astro Ultimate Powerbox v2 Environmental Sensors",
    "enabled": true,
    "averaging_period": "5m"
  }
}
```

Shape, defaults and semantics follow [`ppba-driver`](ppba-driver.md#configuration)
exactly: the shared `AlpacaServerConfig` from `rusty-photon-server-config`
(ADR-016), `deny_unknown_fields` on every block, empty `unique_id` meaning
"mint a UUIDv4 on first run" via `rusty_photon_config::resolve_and_init`,
and humantime durations. The platform default serial port stays the repo's
placeholder convention (`/dev/ttyUSB0` / `COM3`), which the operator edits.

### `AveragePeriod` and the meaning of zero

ASCOM reads `AveragePeriod = 0` as "the device is not averaging — give me the
most recent value". The three sensor means here are window-based and have no
unaveraged mode, and `get_mean` applies its window on *read*, so a literal
zero-length window would answer `VALUE_NOT_SET` at every read. An unbounded
window is no better: it would report an hours-old sample from a stalled poll
loop as current, which is the staleness the read-side window exists to
prevent.

Zero therefore maps to the shortest window that still always holds the newest
sample under healthy polling: `max(3 × serial.polling_interval, 10s)`. Three
intervals tolerates two missed polls before readings degrade to
`VALUE_NOT_SET` — the honest answer once the device has been quiet that long
— and the 10 s floor keeps a fast poll cadence from making the window shorter
than one client round trip.

The period a client sets is stored verbatim and read back as-is, rather than
inferred from the resulting window. Inferring it cannot represent zero (the
window is never zero) and makes a genuine average whose length happens to
equal the instantaneous window indistinguishable from "not averaging".

`config.apply` validates `averaging_period` against the same bounds the device
enforces on `SetAveragePeriod`: no lower bound (zero is meaningful), and a 24
hour ceiling, which is ASCOM's. The two must agree — a period a client can
select at runtime but not persist, or persist but not select, is a trap either
way.

### Config actions

`config.get` / `config.apply` / `config.schema` per
[config-actions.md](config-actions.md), dispatched from either device onto
the one driver config. Secret carried forward: `/server/auth/password_hash`.
Locked identity fields: both `unique_id`s. Hard read-only: `server.port`,
`switch.enabled`, `observingconditions.enabled`. `switch.labels` is editable
and reloads like any other field; a map that breaks one of the four rules
above is rejected as a parse error naming the offending entry, not as a field
error, because the rules are enforced by the label type's own deserializer.

## Error behavior

| Condition | Result |
|-----------|--------|
| Serial port absent at startup | Startup handshake fails; the service restart-loops. Same behavior as the other serial drivers, and the same open design question — see [#1173](https://github.com/rusty-photon/rusty-photon/issues/1173) slice 1b. |
| `P#` answers `PPBA_OK` | Connect fails with a message naming `ppba-driver` as the right service. The prefix check is the model guard. |
| `PA` token count != 21 | `InvalidResponse` naming the count; cache untouched. |
| Any field unparseable | `ParseError` naming the wire field; cache untouched. |
| Write to an auto-dew-controlled channel | `NOT_IMPLEMENTED` naming the channel and the Pegasus software — the classification ASCOM requires of a switch whose `CanWrite` is false. |
| Switch 7 written outside 3-12 | `INVALID_VALUE`; nothing sent to the device. |
| Read before first successful poll | `NOT_CONNECTED`. |
| Sensor read after the averaging window has emptied | `VALUE_NOT_SET`. The window is applied on read, so a stalled poll loop degrades to "no value" rather than reporting an aged-out mean as current. |

## MVP scope

**In:** Switch (39 switches), ObservingConditions (temp / humidity /
dewpoint with the shared sliding-window mean), the read-only handshake and
poll loop, config actions, TLS/auth, doctor, packaging, BDD and ConformU.

**Deferred:** the Focuser device (no motor to validate against); `PZ:`
master off; auto-dew *writes*; boot-state (`PE:`/`US:`) configuration; the
`XS:` external motor controller.

## Relationship to issue #1173

[#1173](https://github.com/rusty-photon/rusty-photon/issues/1173) (daily
power cycling) names "UPBv2 unsupported" as one of the facts its design
rests on, and lists `set_switch`/`get_switch` + UPBv2 as slice 2. This
service is that slice's driver half. Note the tenet-3 boundary the issue
already draws: this driver *exposes* the outputs; deciding to flip one at
dusk is a workflow decision that belongs in a session-runner document, not
in any connect or supervisory path here.

## What the wire actually does

Answered by a read-only probe of rig2's unit
(`FTDIBUS\VID_0403+PID_6015+UPB248E11MA`, firmware `2.4`) sending only
`P#`, `PV`, `PA`, `PC` and `PS`.

**`PA` prefix is `UPB2:`.** The vendor table contradicts itself — its
example line says `UPB:`, its field legend says `UPB2:` — so the parser
accepts both. The box emits `UPB2:`.

**`PC` does not echo.** The reply is the bare tuple
`0.17:14.56:184.93:305389357`, with no `PC:` prefix, so the codec
recognises it structurally: exactly four colon-separated tokens that all
parse as numbers.

**`PS` omits leading zeros, and the vendor table does not say so.** It is
documented as `PS:bbbb:nn` and exampled as `PS:1111:8`, but the box answers
`PS:110:6` — three characters for four outputs. `PA`'s port-status field is
zero-padded (`0010`) in the same exchange, so the two fields go through
different formatting paths in the firmware and only this one loses leading
zeros. The field is therefore read **right-aligned**: `110` is `0110`, `1`
is `0001`, `0` is `0000` — the only reading consistent with the flags being
printed as a number.

This mattered: a fixed-width parse of that field made the handshake fail on
the real device, so the driver could not connect at all. Mock-only testing
could not have caught it, because the mock was written from the same vendor
table. The mock now reproduces the firmware's formatting rather than the
document's.

**Replies are CRLF-terminated**, though the table documents LF. The frame
transport splits on `\n` and the codec trims, so the trailing `\r` costs
nothing — but a future parser that compares untrimmed bytes would break.

**No DTR handshake is needed.** The box answers identically with DTR
asserted and not, unlike the `dsd-fp2`, so the serial layer does not set it.

## Hardware validation

Run against rig2's UPBv2 (`UPB248E11MA`, firmware `2.4`) with the service
binary from the packaged MSI, unpacked with `msiexec /a` and run from a
temporary directory — nothing installed.

The connect handshake succeeds, both devices register, and every Alpaca read
agrees with the raw frames: 12.7 V in, outputs and USB ports matching the
`PA` bit fields, and the variable-output setpoint of 6 V, which is carried
only by `PS` — the frame whose parse used to fail.

All four ConformU suites pass with **zero errors, issues or alerts**:

| Suite | Device | Result |
|-------|--------|--------|
| `conformance` | `ObservingConditions` | clean |
| `alpacaprotocol` | `ObservingConditions` | clean |
| `conformance` | `Switch`, writes enabled | clean |
| `alpacaprotocol` | `Switch` | clean |

Two results are worth keeping:

- The **auto-dew gate is confirmed on hardware.** rig2's box runs auto-dew
  mask `1`, so all three dew channels report `CanWrite = false`, and
  ConformU records `SetSwitch`/`SetSwitchValue` correctly raising
  `NotImplemented` for each. The mock's default has auto-dew off, so this
  path only ever ran under the deliberately-gated ConformU pass — on the
  real box it is the *only* path.
- **Writes round-trip through a real 5 s poll interval.** Every set is
  followed by a cache refresh, so a read straight after a write reflects the
  device rather than the last poll.

Every controllable value — four outputs, three dew duties, six USB ports,
the variable-output setpoint and the auto-dew mask — was captured before the
write pass and compared after: identical. Only the sensors and the energy
counters moved.

## Open items

1. **`PZ:b`.** Excluded above on table-orthogonality grounds. If an operator
   wants a single "everything off" control, the alternative is to expose it
   and document that it mutates seven other switches. Decision pending.

## Testing

Per [testing.md](../skills/testing.md) and the `ppba-driver` precedent:
feature files under `tests/features/`, steps under `tests/bdd/steps/`, the
binary spawned with `--features mock`. 11 features, 230 scenarios, plus 241
unit tests in `src/`.

### The mock's pinned frame

`src/mock.rs` serves one fixed frame, and `sensor_readings.feature` asserts
its values exactly — a scaling or field-order regression has to fail there:

```
PA  UPB2:12.5:2.4:30:25.0:60:16.5:1101:111101:128:64:0:480:960:0:240:240:96:350:0000000:0
PC  1.85:0.42:5.1:3600000          (bare tuple, no prefix)
PS  PS:1101:12
```

The current fields are raw sense counts, so output 1 reads 1.0 A (480/480)
and dew C reads 0.5 A (350/**700**) — the two divisors are covered by
separate scenarios.

Two knobs reach state the driver cannot set, since it never writes `PD:`:

| Environment variable | Effect |
|----------------------|--------|
| `UPBV2_MOCK_AUTO_DEW` | Raw 0-7 auto-dew mask. The BDD suite uses 3 (channel B only) and 1 (all channels) to exercise both sides of the per-channel `CanWrite` gate. |
| `UPBV2_MOCK_OVERCURRENT` | The 7-character overcurrent field, verbatim. |

Both are read once at mock construction and fall back to the default frame
on an unparseable value. `MockUpbv2TransportFactory::with_auto_dew` /
`with_overcurrent` are the in-process equivalents for unit tests, which must
not mutate process-global environment.

ConformU runs against both devices, with auto-dew set to 0 on the hardware
first.

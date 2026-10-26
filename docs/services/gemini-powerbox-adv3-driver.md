# Gemini Power & Data Hubs Advanced 3 Driver

ASCOM Alpaca Switch and ObservingConditions driver for the Gemini Astro
Power & Data Hubs Advanced 3 (the box identifies itself on the wire as
`GeminiPowerBoxPlusAdv3`; the vendor's ASCOM driver is named
`GeminiPowerAndDataHubAdv3`).

**Status: design (Phase 1 of [development-workflow.md](../skills/development-workflow.md)).**
No code exists yet. Everything stated about the wire was either read from
the two open-source drivers named under [Sources](#sources) or measured
against the dev box's own unit on 2026-10-26, and the document says which.

## Overview

The Advanced 3 is a 12 V power and USB distribution box: five 12 V outputs
(one always on, four switched), two dew-heater channels with a PID
auto-dew mode, six switchable USB ports (two USB 3.2 Gen1, four USB 2.0),
an ambient temperature/humidity sensor and a lens-temperature probe, and
input-voltage / output-current / output-power telemetry. This service
exposes it as an ASCOM Alpaca **Switch** device (26 switches) and an
**ObservingConditions** device (temperature, humidity, dewpoint), in the
shape [`ppba-driver`](ppba-driver.md) and [`upbv2-driver`](upbv2-driver.md)
established: one serial port, the shared transport crate, a read-only
handshake, a poll loop into a cache, operator labels, setup pages.

It is a **separate service**, not a mode of either Pegasus driver and not a
"Gemini" driver for the whole product family. The command language is
Gemini's own `>X#` / `*X…#` family, which the vendor also uses on its flat
panels and its focuser — different devices with different command sets and
a different baud rate — and the PowerBox & Hub Mini v2, whose status frame
is unknown. The reasons [`upbv2-driver`](upbv2-driver.md#overview) gives
against one binary for two power boxes apply unchanged: a shared command
enum is a tenet 2 hazard, and `ServerBuilder::build()` registers the
device set before any handshake, so a binary cannot pick its devices by
model. The reuse that matters is at crate level
(`rusty-photon-shared-transport`, `-server-config`, `-config`, `-driver`,
`-rolling-stats`, `-service-lifecycle`, `-setup`, `-tls`, `rp-auth`, `-i18n`,
`-doctor-checks`).

## Hardware

What the vendor's product page states
(https://geminiastro.cc/products/powerbox-adv3), kept here because the
vendor publishes no manual:

| Item | Vendor statement |
|------|------------------|
| Input | 12 V DC, 10 A total across all channels |
| DC1 | always-on 12 V output behind an ideal diode (backflow protection) |
| DC2–DC5 | 12 V outputs, each switchable in software; overcurrent, undervoltage and over-temperature protection with automatic restart |
| DEW6, DEW7 | PWM outputs 0–100 %, each switchable to plain DC; automatic PID control from the two sensors |
| USB A, B | USB 3.2 Gen1 (5 Gbps), switchable |
| USB C–F | USB 2.0, switchable |
| USB power | 5 V / 8 A from a synchronous buck regulator |
| Sensors | AHT20 ambient temperature and humidity (±0.5 °C, ±3 % RH) on the "Temp" port; DS18B20 lens-surface probe on the "Dew Temp" port |
| Telemetry | input voltage, total output current, instantaneous power |
| Firmware | upgradeable over USB with the vendor's `FirmwareUpgradeTool` |

The two sensors work as a pair: the auto-dew mode is a PID loop that holds
the lens probe's temperature above the dew point the AHT20 yields. Auto
mode is only meaningful with both attached.

### Device identity

Transcribed from the dev box's bus on 2026-10-26 (sysfs `idVendor`,
`idProduct`, `manufacturer`, `product`, `serial`), as
[doctor](doctor.md#the-derived-catalog) requires — never from a protocol
reply:

| Property | Value |
|----------|-------|
| Control channel | QinHeng CH340 USB-serial bridge, `1a86:7523` |
| USB product descriptor | `USB Serial` (manufacturer string empty, **no serial number**) |
| Linux by-id link | `usb-1a86_USB_Serial-if00-port0` |
| Serial settings | 19200 baud, 8N1 |
| Firmware baseline | 308 (`3.0.8`); 309 recommended |
| `pkg/doctor.toml` | `usb_vendor = "1a86"`, `usb_product = "7523"`, **no `usb_model`** |

The hub enumerates as several USB functions at once: a VIA Labs VL817 hub
(`2109:0817` on the USB 3 bus, `2109:2817` on the USB 2 bus), a second
`1a86:8091` hub, a VIA `2109:8817` billboard device, and the CH340. Only
the CH340 carries the control channel.

Two consequences of the CH340's stock descriptor:

- **doctor declares no `usb_model`.** `USB Serial` names the bridge chip,
  not the instrument — the same situation as `qhy-focuser`'s `GD32-CDC_ACM`
  and `star-adventurer-gti`'s `STM32 Virtual ComPort`
  ([doctor.md](doctor.md#the-derived-catalog)). The `hardware.usb-device`
  check therefore means "a CH340 is on the bus", which is the honest
  statement; the handshake below is what tells a Gemini hub from any other
  CH340 device.
- **The by-id link is not unit-specific.** Every CH340 without a serial
  number — the Gemini Motorized Flat Panel V3 and the Gemini focuser
  included — gets the same `usb-1a86_USB_Serial-if00-port0` name, and udev
  keeps only one of them. A rig with two such devices configures
  `serial.port` as a `/dev/serial/by-path/…` link (stable per physical USB
  port) or the raw node, not the by-id link.

On Windows the vendor's download page says the CH341SER driver "must be
installed first during initial setup"; the hub then appears as a COM port.
The driver does not bundle it.

## Device protocol

### Link and framing

| Item | Value | Source |
|------|-------|--------|
| Serial | 19200 baud, 8N1, DTR and RTS asserted | both drivers; measured |
| Command | ASCII `>X…#` followed by `\n` | both drivers (the vendor's uses `SerialPort.WriteLine`; INDI sends `\r\n`); measured with `\n` |
| Reply | ASCII `*X…#` where `X` echoes the command letter; **no line ending after `#`** | measured: frames arrive back to back, `…P#*G…` |
| Frame terminator for `SerialFrameTransport` | `#` | follows from the above |
| Set commands | produce **no reply** | both drivers |

The box speaks 19200 only. The configuration has no `baud_rate` key: a
field whose single valid value is fixed by the firmware would be a trap, and
`deny_unknown_fields` makes the key a load error if someone adds it.

### Commands

| Command | Reply | Meaning |
|---------|-------|---------|
| `>H#` | `*HGeminiPowerBoxPlusAdv3#` | Identity. Compared verbatim. |
| `>V#` | `*V<nnn>#` | Firmware as a 3-digit integer: `308` is 3.0.8. |
| `>G#` | `*G<frame>#` | Full status frame ([layout](#status-frame-g)). |
| `>O<n>#` | none | Output `n` on, `n` = 1..11 ([numbering](#output-numbering)). |
| `>C<n>#` | none | Output `n` off. |
| `>X<pct>#` | none | DEW6 manual duty, 0..100. |
| `>Y<pct>#` | none | DEW7 manual duty, 0..100. |
| `>Z10#` / `>Z11#` | none | DEW6 disabled / enabled (acts in Auto and Switch modes). |
| `>Z20#` / `>Z21#` | none | DEW7 disabled / enabled. |
| `>M10#` / `>M11#` / `>M12#` | none | DEW6 mode Auto / Manual / Switch. |
| `>M20#` / `>M21#` / `>M22#` | none | DEW7 mode Auto / Manual / Switch. |

#### Output numbering

| `n` | Output |
|-----|--------|
| 1 | DC1 — always on. **Never sent** by this driver (see below). |
| 2–5 | DC2–DC5, 12 V switched |
| 6–11 | USB A–F |

The dew channels are numbered 6 and 7 on the box's panel and in the
vendor's names (`DEW6`, `DEW7`), but on the wire they are channel `1`
(`>Z1x#`, `>M1x#`, `>X`) and channel `2` (`>Z2x#`, `>M2x#`, `>Y`). Outputs
6–11 of `>O`/`>C` are the USB ports, not the dew channels. The protocol
module owns this mapping so no caller ever composes an output number.

#### Commands the driver never sends

| Command | Why |
|---------|-----|
| `>O1#`, `>C1#` | DC1 is sold as always-on and the vendor's own driver never addresses it. What the firmware does with output 1 is unknown, and nothing on the ASCOM surface needs it. |

### Status frame (`>G#`)

The reply to `>G#` is one frame of seventeen fields. Every field is a
number followed by a **suffix tag letter**; the numeric fields are
fixed-width with leading spaces. Tags repeat (`A`, `M`, `C`, `D`, `T` each
occur twice), so the parser consumes the fields **in order**, each up to
its expected tag, the way the INDI driver does — not by splitting on the
tag alphabet.

Captured from the dev box's unit (firmware 308) rather than copied from
either driver:

```
*G1111D111111U1A1T1A1M1B1M100C100C 23.25S 24.47T 67.76H 18.12D12.2V 0.02C  0.36P#
```

| # | Tag | Field | Value above | Notes |
|---|-----|-------|-------------|-------|
| 0 | `D` | DC2–DC5 state | `1111` | one digit per output, `1` = on |
| 1 | `U` | USB A–F state | `111111` | one digit per port |
| 2 | `A` | AHT20 attached | `1` | `0`/`1` |
| 3 | `T` | DS18B20 attached | `1` | `0`/`1` |
| 4 | `A` | DEW6 enabled | `1` | the `>Z1x#` flag |
| 5 | `M` | DEW6 mode | `1` | `0` Auto, `1` Manual, `2` Switch |
| 6 | `B` | DEW7 enabled | `1` | |
| 7 | `M` | DEW7 mode | `1` | |
| 8 | `C` | DEW6 duty | `100` | percent |
| 9 | `C` | DEW7 duty | `100` | percent |
| 10 | `S` | lens temperature | ` 23.25` | °C, DS18B20 |
| 11 | `T` | ambient temperature | ` 24.47` | °C, AHT20 |
| 12 | `H` | humidity | ` 67.76` | % RH, AHT20 |
| 13 | `D` | dew point | ` 18.12` | °C, computed by the hub |
| 14 | `V` | input voltage | `12.2` | V, one decimal |
| 15 | `C` | output current | ` 0.02` | A |
| 16 | `P` | output power | `  0.36` | W |

The frame is 83 bytes, which is 43 ms on the wire at 19200 baud — long
enough that it arrives from the kernel in several reads, and long enough to
matter for the write path below.

Sanity checks on the capture: 24.47 °C at 67.76 % RH gives a Magnus dew
point of 18.1 °C, which is what field 13 says, so the `T`/`H`/`D` trio is
the AHT20 and `S` is the probe; and 0.03 A × 12.2 V ≈ 0.36 W, so `C` and
`P` are the same measurement in two units.

### What the wire actually does

Measured on 2026-10-26 against the dev box's unit with a read-only probe
that sent only `>H#`, `>V#` and `>G#` (`/dev/ttyUSB0`, Linux `ch341`
driver, 19200 8N1, DTR and RTS raised). Everything in this section is an
observation; the drivers' claims it confirms or contradicts are named.

- **Identity and firmware.** `>H#` answered `*HGeminiPowerBoxPlusAdv3#`
  118 ms after the send; `>V#` answered `*V308#`. The dev box's unit runs
  the oldest firmware the vendor accepts.
- **A `>G#` is answered at once.** The frame's first byte arrived 21–67 ms
  after the send, on every attempt, with no echo of the command and no
  line ending.
- **The hub streams frames on its own once asked.** Nothing arrived during
  three seconds of silence after the open. After the first `>G#` reply the
  hub emitted a `*G` frame every **3.000 s**, unprompted, for as long as the
  probe listened — the behaviour AlpacaBridge found on firmware 309. A
  later `>G#` is answered immediately **and the periodic frame still comes
  on the old schedule**: the stream keeps one phase (every frame in the run
  landed at the same offset modulo 3 s) and a request does not re-phase it.
- **The CH340's DTR edge does not reset this unit.** AlpacaBridge records
  that opening the port "pulses the MCU reset line" and keeps DTR high
  across close (`HUPCL` cleared) to avoid a second reset. Measured here:
  closing with DTR held and reopening, closing with DTR dropped and
  reopening — in all four open/close cycles the stream continued on its
  original phase, the next frame arriving within 0.5 s of the open. A
  reset would have stopped the stream (it needs a fresh `>G#` to arm) and
  the phase would have moved. The driver therefore does nothing special
  with DTR or `HUPCL`; the `>H#` retry below is the only concession to a
  unit that might behave differently.
- **The hub's state when found.** DC2–DC5 all on, USB A–F all on, both
  sensors attached, both dew channels **Manual at 100 %**, 12.2 V in,
  0.02–0.06 A. AlpacaBridge reports the same dew state on power-up
  ("both dew channels came up Manual at 100 %"). Whether the hub persists
  output states across its own power cycle was not tested; what the capture
  does say is that **a heater plugged into DEW6 or DEW7 runs at full power
  from the moment the hub powers up until some client changes it.** That
  is a property of the hub, not of this driver, and the operator needs to
  know it.

### Sources

Neither the vendor nor anyone else publishes a protocol document. Two
independent reverse-engineered drivers agree with each other and with the
capture above:

- **INDI**, `drivers/power/indi-gemini-pbh/` in `indilib/indi`, merged
  2026-08-30 ([PR #2469](https://github.com/indilib/indi/pull/2469)),
  GPL-2.0-or-later, validated on firmware 308 hardware. Its
  `parseStatusPayload` is the in-order, per-tag parser described above.
  Its usage guide carries the sensor-presence rule for Auto mode and the
  firmware gate (308 rejected below, 309 recommended for a DS18B20
  detection fix).
- **AlpacaBridge** (`open-astro/AlpacaBridge`),
  `AlpacaCore/external/Gemini/PowerDataHubAdv3-protocol.md`, a summary of
  the vendor's own Windows ASCOM driver
  (`ASCOM.GeminiPowerBoxPlusAdv3.Switch.dll` v2.6.0206, decompiled),
  confirmed on firmware 309 hardware on 2026-09-08. It is the source for
  the vendor's switch surface, the output numbering, and the vendor
  driver's own behaviours (2 s post-open sleep, exact identity compare,
  refusal below 308, fire-and-forget sets, forcing a channel to Manual
  when a sensor is missing).

No code is taken from either; the protocol is a wire format, and both
sources are cited so a later reader can check a claim against them.

## ASCOM device set

| Device | Status |
|--------|--------|
| Switch | MVP |
| ObservingConditions | MVP |

One serial port, one command stream, sensors reachable only through the
hub: bundling the two devices in one service is the
[ADR-014](../decisions/014-zwo-per-device-services-and-link-features.md)
case for bundling, exactly as for the two Pegasus boxes.

## Switch mapping

**MaxSwitch = 26.** Ids are contiguous from zero and stable; the enum
variant order is not the id (`SwitchId::info().id` is), matching the
siblings. The table is ordered the way the box's panel is: DC outputs, USB
ports, dew channels, then telemetry.

### Writable (CanWrite = true, with the dew exceptions below)

| Id | Name | Command | Min | Max | Step | Notes |
|----|------|---------|-----|-----|------|-------|
| 0 | `DC1` | — | 0 | 1 | 1 | **Read-only, always 1.** The always-on rail; listed so the panel is complete and its connector can be labelled. |
| 1–4 | `DC2` … `DC5` | `>O2#`/`>C2#` … `>O5#`/`>C5#` | 0 | 1 | 1 | |
| 5–10 | `USB A` … `USB F` | `>O6#`/`>C6#` … `>O11#`/`>C11#` | 0 | 1 | 1 | A and B are the USB 3.2 ports |
| 11 | `DEW6 Duty` | `>X<pct>#` | 0 | 100 | 1 | writable in **Manual** mode only |
| 12 | `DEW7 Duty` | `>Y<pct>#` | 0 | 100 | 1 | writable in **Manual** mode only |
| 13 | `DEW6 Enabled` | `>Z10#`/`>Z11#` | 0 | 1 | 1 | writable in **Auto** and **Switch** modes only |
| 14 | `DEW7 Enabled` | `>Z20#`/`>Z21#` | 0 | 1 | 1 | writable in **Auto** and **Switch** modes only |
| 15 | `DEW6 Mode` | `>M10#`/`>M11#`/`>M12#` | 0 | 2 | 1 | `0` Auto, `1` Manual, `2` Switch |
| 16 | `DEW7 Mode` | `>M20#`/`>M21#`/`>M22#` | 0 | 2 | 1 | same |

### Read-only (CanWrite = false)

| Id | Name | Source | Min | Max | Step |
|----|------|--------|-----|-----|------|
| 17 | `Input Voltage` | frame 14 | 0 | 19 | 0.1 |
| 18 | `Output Current` | frame 15 | 0 | 15 | 0.01 |
| 19 | `Output Power` | frame 16 | 0 | 180 | 0.01 |
| 20 | `Temperature` | frame 11 (AHT20) | -127 | 60 | 0.01 |
| 21 | `Humidity` | frame 12 (AHT20) | 0 | 100 | 0.01 |
| 22 | `Dewpoint` | frame 13 | -127 | 60 | 0.01 |
| 23 | `Lens Temperature` | frame 10 (DS18B20) | -127 | 60 | 0.01 |
| 24 | `Ambient Sensor Attached` | frame 2 | 0 | 1 | 1 |
| 25 | `Lens Probe Attached` | frame 3 | 0 | 1 | 1 |

The voltage, current and power ceilings are the vendor driver's (19 V,
15 A, 180 W); the box is rated 10 A at 12 V. The voltage step is 0.1
because the frame carries one decimal there and two everywhere else.

### Dew heaters: three switches per channel

Each dew channel has a mode, a manual duty and an enable flag on the wire,
and the mode decides which of the other two means anything:

| Mode | What the hub does | Duty switch | Enabled switch |
|------|-------------------|-------------|----------------|
| `0` Auto | PID loop holds the lens probe above the dew point | read-only (reports what the hub reports) | writable: `0` stops the loop, `1` runs it |
| `1` Manual | heater at the manual duty | writable, 0–100 | read-only |
| `2` Switch | heater as plain DC | read-only | writable: `0` off, `1` on |

The vendor folds duty and flag into one switch whose range changes with
the mode (0–100 in Manual, 0–1 otherwise). A switch whose
`MaxSwitchValue` moves at runtime is legal but a trap for every client
that caches metadata, so this driver keeps the three quantities as three
switches with fixed ranges and moves the *writability* instead — the
`CanWrite` pattern [`ppba-driver`](ppba-driver.md#auto-dew-behavior)
already uses for its auto-dew gate, and for the same reason: ASCOM ties
`SetSwitch`/`SetSwitchValue` to `CanWrite`, so a write to a switch that is
read-only in the current mode answers `NOT_IMPLEMENTED`, with a message
naming the mode switch to change first. The check reads the mode from the
cache and refreshes it first when the cache is empty, as the PPBA does.

Rules for the mode switch:

- A write of `0` (Auto) while either sensor's attached flag is `0` is
  refused with `INVALID_OPERATION` naming the missing sensor, and nothing
  is sent. The vendor's driver instead forces such a channel back to
  Manual behind the operator's back; an explicit refusal tells them why.
- A write is confirmed against the readback frame like every other write
  ([below](#writes-are-confirmed-against-a-fresh-frame)). If the firmware
  declines a mode — AlpacaBridge notes the vendor driver re-sends `>M10#`
  when the frame does not show Auto — the mismatch is reported, not hidden.
- **The mode is never persisted by this driver and never re-applied.** The
  vendor driver remembers the mode in its own settings and pushes it on
  every connect; that is actuation on connect
  ([tenet 3](../workspace.md#project-tenets)) and is exactly what this
  driver does not do. After a hub power cycle the channels are back at the
  hub's own default (Manual, 100 %), and the driver reports that.

`DEW6 Enabled` is deliberately not named "on": in Auto mode a `1` means
the loop is running, and the heater may well be at 0 % because the lens is
already warm enough.

### Sensor presence and the absent-sensor sentinel

The frame says whether each sensor is attached (fields 2 and 3), and the
driver believes the flag over the number. When a flag is `0`:

- the ObservingConditions device receives **no sample** for that sensor's
  properties, so they read `VALUE_NOT_SET` once the window has emptied
  (ambient temperature, humidity and dewpoint follow the AHT20; nothing in
  ObservingConditions follows the probe);
- the matching Switch rows read a **sentinel**: `-127` for the three
  temperature rows and `0` for humidity. A Switch has no "not set", every
  row must read inside its declared range, and the flags (switches 24 and
  25) are there so a client can explain the sentinel rather than chart it.
  `-127` is the DS18B20 library convention for a disconnected probe, which
  is why the temperature rows declare `-127` as their minimum.

What the firmware itself writes into fields 10–13 while a sensor is
unplugged is not known; neither driver says and the dev box's unit had both
attached. It is on the [hardware validation list](#hardware-validation).

### Operator labels

A port number is what the box knows; what is plugged into it is what the
operator knows. `switch.labels` carries that fact across, with the shape and
the three rules [`upbv2-driver`](upbv2-driver.md#operator-labels)
documents (keys are built-in names, a label needs content, the 26 names
stay unique) and the same shared `SwitchLabels` type.

```json
"switch": {
  "name": "Gemini Advanced 3 Switch",
  "labels": {
    "DC2": "Mount",
    "DC5": "Camera",
    "DEW6": "Guide scope heater",
    "USB A": "Imaging camera"
  }
}
```

The labellable names are the **connectors**: `DC1` … `DC5`, `DEW6`, `DEW7`,
`USB A` … `USB F`. A label follows its connector's rows: labelling `DEW6` as
`Guide scope heater` renames `DEW6 Duty`, `DEW6 Enabled` and `DEW6 Mode` to
`Guide scope heater Duty` / `Enabled` / `Mode`, the way a UPBv2 port label
renames the port's current and overcurrent rows. `DC1` is read-only but it
is a connector, so it takes a label. The telemetry and sensor rows keep
their names. `GetSwitchDescription` is untouched and still names the
physical connector.

Labels are edited in the file and applied by a reload; `SetSwitchName`
stays `NOT_IMPLEMENTED`, as in the siblings.

## Connect, handshake and polling

Tenet 3 (**no actuation on connect**) governs this box as directly as it
governs the UPBv2: almost every write it accepts is a power or heater
change. Therefore:

- The handshake is **read-only**: `>H#` → `>V#` → `>G#`. It validates the
  unit, gates the firmware, and seeds the cache. It re-runs on every
  reconnect after a serial glitch, so it must stay read-only by
  construction.
- A setup-page save never pushes output states or dew modes to the hub.
- No boot-state command exists in this protocol, so there is no path by
  which a configured state could reach the hardware; the dew mode is not
  stored by the driver at all ([above](#dew-heaters-three-switches-per-channel)).
- The DTR edge on open was measured harmless on the dev box's unit
  ([What the wire actually does](#what-the-wire-actually-does)).

A hub that is powered with every output off is a normal, connectable
state; the driver reports it and changes nothing.

### Handshake

1. `>H#`. The reply must be `*HGeminiPowerBoxPlusAdv3#`, compared
   verbatim. Any other `*HGeminiPowerBox…#` is reported as "a Gemini power
   box, but not the Advanced 3"; a `*HGeminiFlatPanel…#` as the vendor's
   flat panel; anything else as not a Gemini device. The guard matters
   because every Gemini CH340 device enumerates identically
   ([Device identity](#device-identity)).
   If nothing answers within `serial.timeout`, the driver waits 2 s and
   sends `>H#` once more before failing — the vendor driver's flat 2 s
   post-open sleep, spent only when needed. The dev box's unit answers at
   once; the retry covers a unit that does reset on the DTR edge.
2. `>V#`. Below 308 the connect is refused, naming the version read and
   the vendor's upgrade tool; the frame layout is only known for 3.0.8 and
   later and both open-source drivers refuse likewise. 308 connects with a
   `warn!` that 309 fixes DS18B20 detection. The value is kept for
   `DriverInfo`.
3. `>G#`. The first frame seeds the cache. A reply that is not a parseable
   `*G` frame fails the connect.

### The poll loop lives with the stream

Once armed, the hub streams a frame every 3 s whether or not anyone reads
it. The driver **does not consume the stream as its poll**; it discards it
and asks for a fresh frame on its own cadence. The reason is the shared
transport's request model — one command, one lock, read until the matching
reply — which gives every sibling its cache-refresh semantics and its
write-then-readback guarantee. A streamed frame sitting in the kernel
buffer would otherwise be taken as the answer to a `>G#` sent seconds
later, and after a write it would show the state *before* the write.

So every request that expects a `*G` frame is preceded by a **discard of
the input buffer**, which is one small addition to the shared crate:

- `FrameTransport::discard_input()` — default no-op; `SerialFrameTransport`
  implements it as a `ClearBuffer::Input` on the port plus dropping its own
  partial-frame buffer; the BDD mock drops its queued unsolicited frames.
- `Codec::discard_before_send(&cmd) -> bool` — default `false`; the Gemini
  codec answers `true` for `>G#` and for every set command.
- `Connection::request` calls the first when the second says so, right
  before `send_frame`.

What can still precede the real reply after a discard: the tail of a frame
that was mid-flight when the buffer was cleared (it does not start with
`*`), and at most one whole periodic frame whose first byte landed after
the discard. The codec decodes anything that is not a parseable `*G`, `*H`
or `*V` frame into an `Unrelated` response rather than an error — the
request loop treats a decode error as fatal, but a non-matching response
as a skip — and `matches` rejects `Unrelated` and any `*G` frame when the
request was not for one. `max_skip` is 2.

The poll loop then is: every `serial.polling_interval` (default 5 s),
discard, `>G#`, parse, store in the shared `CachedState`. Reads are served
from the cache; a poll that fails to parse leaves the cache untouched and
logs at `debug!`; a poll that times out is a transport error and the
shared crate's supervisor reconnects (re-running the read-only handshake).

### Writes are confirmed against a fresh frame

Set commands get no acknowledgement, and the frame is the only evidence a
write took. Every write is therefore **one request carrying two commands**:
the set command and a `>G#`, encoded back to back in one buffer
(`>O3#\n>G#\n`), after a discard. The response is the frame the hub
produced after acting on the set, and the driver compares the field the
write addressed with what it asked for:

- a match stores the frame in the cache and the write succeeds — so a read
  straight after a write reflects the device, as in the siblings;
- a mismatch is asked once more (discard, `>G#`) before it is believed,
  because a periodic frame generated during the 5 ms the two commands take
  to go out predates the set and is the first frame read about 1.4 % of the
  time (43 ms of frame per 3 s of period);
- a mismatch that survives the second look is reported as
  `INVALID_OPERATION`, naming the switch, the value asked for and the value
  the hub reports — a tripped output that the hub refuses to turn back on
  surfaces here, not as a silent success.

Which field confirms which write:

| Write | Confirmed by |
|-------|--------------|
| DC2–DC5, USB A–F | frame 0 / 1 digit |
| `DEWn Enabled` | frame 4 / 6 |
| `DEWn Mode` | frame 5 / 7 |
| `DEWn Duty` | frame 8 / 9 — **provisional**: the INDI driver classes the manual duty as *not confirmable*, without saying why. If hardware validation shows fields 8/9 do not follow `>X`/`>Y` promptly, the duty write becomes send-and-record (the cache holds the commanded value until the next frame), and this table is amended. |

The two-commands-in-one-buffer shape has not been exercised on hardware;
both open-source drivers send sets alone. It is the first item on the
[hardware validation list](#hardware-validation), and the fallback is two
requests under the same discard rule, which costs one more lock cycle and
nothing else.

### Synchronous switches only

Every switch reports `CanAsync = false`; `SetAsync`, `SetAsyncValue` and
`StateChangeComplete` answer `NOT_IMPLEMENTED`, `CancelAsync` succeeds as a
no-op, all four check the connection and the id first, and `DeviceState`
omits `StateChangeComplete{n}` — the ISwitchV3 shape
[`ppba-driver`](ppba-driver.md#architecture) documents under "Synchronous
switches only".

### Reload gives the port back before it takes it again

Identical to the siblings: a reload (setup-page save, `SIGHUP`, SCM
`ParamChange`) drains the server, closes the transport, and rebuilds with a
fresh handshake on the same port; `shutdown()`'s explicit close and
`open_serial_port`'s bounded reopen retry are what make that work, and
`tests/reload_port_release.rs` pins it
([upbv2-driver](upbv2-driver.md#reload-gives-the-port-back-before-it-takes-it-again)).
Because the stream survives a close, the first frame after a reopen can be
a stale one; the handshake's discard-before-`>G#` covers that like any
other request.

## ObservingConditions

`Temperature`, `Humidity` and `DewPoint` are the AHT20's three values from
the poll loop, each through the shared sliding-window mean of
[`rusty-photon-rolling-stats`](../crates/rusty-photon-rolling-stats.md).
`AveragePeriod`, the meaning of zero, the `[0, 24]` hour range, the `NaN`
rejection, the `MAX_AVERAGING_PERIOD` constant shared by the device, the
manager and the setup page, and the staleness rule (an emptied window
reads `VALUE_NOT_SET`) are exactly as
[`ppba-driver`](ppba-driver.md#averageperiod-staleness-and-the-meaning-of-zero)
specifies them; this document does not restate them. A sample is fed only
while the AHT20's attached flag is `1`
([Sensor presence](#sensor-presence-and-the-absent-sensor-sentinel)).

The lens probe has no ObservingConditions property; it is Switch row 23.
`DewPoint` is the hub's computation, not the driver's, as with the Pegasus
boxes.

## Configuration

```json
{
  "serial": {
    "port": "/dev/ttyUSB0",
    "polling_interval": "5s",
    "timeout": "2s"
  },
  "server": { "port": 11132, "bind_address": "0.0.0.0", "tls": null, "auth": null },
  "switch": {
    "name": "Gemini Advanced 3 Switch",
    "unique_id": "",
    "description": "Gemini Astro Power & Data Hubs Advanced 3 Power Control",
    "enabled": true,
    "labels": { "DC2": "Mount" }
  },
  "observingconditions": {
    "name": "Gemini Advanced 3 Weather",
    "unique_id": "",
    "description": "Gemini Astro Power & Data Hubs Advanced 3 Environmental Sensors",
    "enabled": true,
    "averaging_period": "5m"
  }
}
```

Shape, defaults and semantics follow
[`ppba-driver`](ppba-driver.md#configuration): the shared
`AlpacaServerConfig` (ADR-016), `deny_unknown_fields` on every block, an
empty `unique_id` meaning "mint a UUIDv4 on first run" via
`rusty_photon_config::resolve_and_init`, humantime durations, the bootstrap
that fills in an absent device section and never writes a file the driver
would refuse. Two differences:

| Field | Here |
|-------|------|
| `serial.baud_rate` | **absent**. The hub speaks 19200 only ([Link and framing](#link-and-framing)). |
| `serial.port` default | `/dev/ttyUSB0` on Unix, `COM3` on Windows — the repo's placeholder convention, which the operator edits. On a rig with a second CH340 device, a `by-path` link ([Device identity](#device-identity)). |

| Section | Field | Description | Default |
|---------|-------|-------------|---------|
| serial | port | Serial port path | placeholder, see above |
| serial | polling_interval | Status poll interval (humantime) | `"5s"` |
| serial | timeout | Reply timeout per request (humantime) | `"2s"` |
| server | port | HTTP server port | 11132 |
| server | bind_address | Interface to bind | `"0.0.0.0"` |
| server.auth | username / password_hash | HTTP Basic Auth (optional) | — |
| switch | name, unique_id, description, enabled, labels | as `ppba-driver` | `"Gemini Advanced 3 Switch"`, minted, as shown, `true`, absent |
| observingconditions | name, unique_id, description, enabled, averaging_period | as `ppba-driver` | `"Gemini Advanced 3 Weather"`, minted, as shown, `true`, `"5m"` |

### Setup pages

The three pages `ppba-driver` serves, with the same division of labour
between the driver and [`rusty-photon-setup`](../crates/rusty-photon-setup.md):

| Path | Holds |
|------|-------|
| `/setup` | `serial` (port, polling interval, timeout), `server`, the Security section, the device list |
| `/setup/v1/switch/0/setup` | the `switch` section; `labels` named as edited in the file |
| `/setup/v1/observingconditions/0/setup` | the `observingconditions` section |

Command-line flags pin `serial.port` (`--port`), `server.port`
(`--server-port`), `switch.enabled` (`--enable-switch`) and
`observingconditions.enabled` (`--enable-observingconditions`); a pinned
field shows disabled and is never persisted. A save never pushes anything
to the hub. The driver has no custom Actions.

#### Validation rules

| Field | Rule |
|-------|------|
| `serial.port` | non-empty (trimmed) |
| `serial.polling_interval` | `> 0` |
| `serial.timeout` | `> 0` |
| `observingconditions.averaging_period` | `<= 24h`; zero allowed |
| `switch.unique_id`, `observingconditions.unique_id` | non-empty |
| `switch.labels` | the three label rules, at deserialize |
| `server.auth` | a username needs a password, submitted or stored; the repeat must match |

## Error behaviour

| Condition | Result |
|-----------|--------|
| Serial port absent at startup | Startup handshake fails; the service restart-loops — the siblings' behaviour and the open question of [#1173](https://github.com/rusty-photon/rusty-photon/issues/1173) slice 1b. |
| `>H#` answers another `*HGeminiPowerBox…#` | Connect fails: "a Gemini power box, but not the Advanced 3". |
| `>H#` answers `*HGeminiFlatPanel…#` or anything else | Connect fails naming what answered; the port holds another Gemini device or no Gemini device. |
| `>H#` unanswered | One retry after 2 s, then connect fails with a timeout. |
| `>V#` below 308 | Connect refused, naming the version and the vendor's `FirmwareUpgradeTool`. |
| `>V#` is 308 | Connects; `warn!` that 309 fixes DS18B20 detection. |
| `*G` frame with a tag out of sequence, a wrong digit count, or an unparseable number | `ParseError` naming the field; cache untouched; the request fails and the poll goes on. |
| Frame not starting with `*` (a discarded frame's tail) or a `*G` frame when another reply was asked for | Skipped under the skip budget (2); the request waits for the next frame. |
| Skip budget exhausted | The request fails as a communication error; the cache is untouched. |
| `>G#` unanswered within `serial.timeout` | Transport timeout; the shared supervisor reconnects with the read-only handshake. |
| Write to `DC1` | `NOT_IMPLEMENTED` (CanWrite false). |
| Write to `DEWn Duty` outside Manual, or `DEWn Enabled` in Manual | `NOT_IMPLEMENTED`, naming `DEWn Mode` as the switch to change first; nothing sent. |
| `DEWn Mode` written to `0` while a sensor is absent | `INVALID_OPERATION` naming the missing sensor; nothing sent. |
| `DEWn Mode` outside 0–2, `DEWn Duty` outside 0–100, a non-integer on either | `INVALID_VALUE`; nothing sent. |
| Readback frame does not show the write, twice | `INVALID_OPERATION` naming the switch, the value asked and the value reported. |
| `SetAsync`, `SetAsyncValue`, `StateChangeComplete` on a valid id | `NOT_IMPLEMENTED`; nothing sent. `CancelAsync` succeeds as a no-op. |
| Any member with an id outside 0–25 | `INVALID_VALUE` (or `NOT_CONNECTED` while disconnected), checked before `NOT_IMPLEMENTED`. |
| `SetAveragePeriod` given `NaN` or a value outside `[0, 24]` hours | `INVALID_VALUE`; windows and the recorded period untouched. |
| Read before the first successful poll | `NOT_CONNECTED`. |
| Sensor read after the window has emptied, or while the AHT20 is absent | `VALUE_NOT_SET`. |

## MVP scope

**In:** Switch (26 switches), ObservingConditions (temperature, humidity,
dewpoint through the shared window), the read-only handshake with the
model and firmware gates, the discard-then-poll loop, writes confirmed
against the readback frame, operator labels, setup pages, TLS/auth, doctor
metadata, Linux/macOS/Windows packaging in the siblings' shape, BDD against
a mock that streams, ConformU on the mock, and a real-hardware record on the
dev box's unit.

**Deferred:** the PowerBox & Hub Mini v2 (a separate service when a unit
exists; its frame is unknown); `>O1#`/`>C1#`; consuming the hub's stream as
the poll instead of discarding it (a receive-only request would hold the
transport lock across the 3 s cadence and delay writes — not worth it while
`>G#` answers in 20 ms); any persistence of dew modes.

## Relationship to issue #1173

[#1173](https://github.com/rusty-photon/rusty-photon/issues/1173) (daily
power cycling) wants a Switch to flip outputs at dusk and dawn. This
driver exposes the outputs; the decision to flip one belongs to a
session-runner document, never to any connect or supervisory path here —
the same boundary [`upbv2-driver`](upbv2-driver.md#relationship-to-issue-1173)
draws.

## Testing

Per [testing.md](../skills/testing.md) and the `ppba-driver` precedent:
feature files under `tests/features/`, steps under `tests/bdd/steps/`, the
binary spawned with `--features mock`, unit tests inline in `src/`.

### Feature files (Phase 2)

| Design section | Feature file | Covers |
|----------------|--------------|--------|
| Handshake | `connection_lifecycle.feature` | connect/disconnect over HTTP; identity mismatch (flat panel, other power box, garbage); firmware 307 refused, 308 warns, 309 clean; `>H#` retry |
| Switch mapping | `switch_metadata.feature` | 26 names, descriptions, ranges, steps, `CanWrite` per dew mode, DC1 read-only |
| Writes | `switch_control.feature` | each writable switch round-trips through the readback frame; dew mode gates; Auto refused without sensors; the stale-frame second look |
| Error behaviour | `switch_errors.feature` | the table above, row by row |
| Status frame | `sensor_readings.feature` | the pinned frame's values read back exactly; every row inside its range; sentinel rows with a sensor absent |
| ObservingConditions | `observing_conditions.feature` | the three means, `AveragePeriod`, `VALUE_NOT_SET` on an empty window and on an absent AHT20 |
| Labels | `switch_metadata.feature` | a `DEW6` label renames three rows; a bad key fails the load |
| Config, setup, auth, TLS, doctor, server registration | as the siblings' files | |

### The mock's pinned frame

`src/mock.rs` serves the frame captured from the dev box's unit, so a
scaling or field-order regression fails `sensor_readings.feature`:

```
H   *HGeminiPowerBoxPlusAdv3#
V   *V308#
G   *G1111D111111U1A1T1A1M1B1M100C100C 23.25S 24.47T 67.76H 18.12D12.2V 0.02C  0.36P#
```

The mock **streams**: after the first `>G#` it queues a frame every 3 s of
mock time, and when a knob says so it also queues a pre-request frame ahead
of its reply, so the skip path is exercised deterministically rather than
by chance. Its `discard_input` drops the queue; a scenario
asserts that a write's readback is the post-write frame even with a
pre-write frame queued ahead of it. Set commands change the mock's state
and produce no reply; the mock applies `>X`/`>Y` to fields 8/9 at once
until hardware says otherwise.

Knobs reaching state the driver cannot set:

| Environment variable | Effect |
|----------------------|--------|
| `GEMINI_MOCK_FIRMWARE` | the `>V#` reply, e.g. `307` |
| `GEMINI_MOCK_IDENTITY` | the `>H#` reply, for the mismatch scenarios |
| `GEMINI_MOCK_SENSORS` | two flags, `10` = AHT20 only, for the sentinel and the Auto-refusal scenarios |
| `GEMINI_MOCK_DEW_MODES` | the two mode digits at start, e.g. `01` |

Each is read once at mock construction; the in-process `with_…` builders
are the unit-test equivalents.

### ConformU

Two Switch passes on the mock, as `ppba-driver` runs: one with both
channels in Manual (ConformU drives the duty switches 0–100 and finds the
enable switches read-only), one with both in Auto (the reverse). Both
ObservingConditions suites once. Reduced delays via
`bdd_infra::FullRunSettings` as the siblings.

On real hardware ConformU **writes every writable switch**: it turns
DC2–DC5 and USB A–F off and on, drives both dew duties across 0–100, and
cycles both modes through Auto, Manual and Switch. Run it with nothing on
the DC, USB and DEW ports that minds — a camera on a USB port loses its
link, a heater on a DEW port goes to full power in Switch mode
([hardware-validation.md](../skills/hardware-validation.md) says the same
of every powerbox).

### Hardware validation

The first validation run on the dev box's unit (firmware 308) settles the
facts this design could not, each recorded in the run's
`docs/validation/` entry and folded back into this document:

1. **Two commands in one buffer.** Does `>O3#\n>G#\n` yield a frame
   showing output 3 changed? If the firmware drops the second command, the
   write path falls back to two requests.
2. **Fields 8/9 under `>X`/`>Y`.** Does the duty field follow the set at
   once in Manual mode, and what does it show in Auto mode — the manual
   setpoint or the loop's live duty?
3. **Absent sensors.** The frame with the AHT20 unplugged, and with the
   DS18B20 unplugged: fields 2/3 and whatever the firmware puts in 10–13.
4. **Dew modes.** That `>M1x#` is reflected in field 5, that `>Z1x#` is
   reflected in field 4 in Auto and Switch modes, and what `>Z10#` does in
   Manual.
5. **Output state across a hub power cycle.** Whether DC2–DC5 and USB come
   back as they were or all on. Not a driver behaviour, but an operator
   fact for this document.
6. **ConformU**, all four suites, clean, in both dew modes, with the
   version gate first.

## Open items

1. **`DEWn Duty` confirmation** — provisional until item 2 above.
2. **The absent-sensor sentinel** — `-127`/`0` is this design's choice
   pending item 3; if the firmware's own value is a usable sentinel, the
   rows may publish it instead.
3. **A second Gemini CH340 device on the same rig** shares the by-id name;
   the design points operators at `by-path`. Whether doctor should warn
   when two `1a86:7523` devices are present is a doctor question, not
   raised as an issue yet.

## Dependencies

- `ascom-alpaca` — Alpaca server, Switch and ObservingConditions traits
- `rusty-photon-shared-transport` — refcounted transport, `open_serial_port`, and the `discard_input` addition this driver introduces
- `rusty-photon-rolling-stats` — the sensor means
- `rusty-photon-config`, `-server-config`, `-setup`, `-driver`, `-service-lifecycle`, `-tls`, `rp-auth`, `-i18n`, `-doctor-checks` — as the siblings
- `tokio`, `serde`/`serde_json`, `clap`, `tracing`, `thiserror`, `strum`

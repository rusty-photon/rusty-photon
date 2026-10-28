# gemini-powerbox-adv3-driver on Linux: Gemini Power & Data Hubs Advanced 3, 2026-10-26

This is the driver's first recorded ConformU run against real hardware: the dev
box's Gemini Astro Power & Data Hubs Advanced 3 on firmware 3.0.8, the unit the
[design doc](../../services/gemini-powerbox-adv3-driver.md) was measured against.
It covers items 1, 2, 4 and 6 of the design doc's
[hardware validation list](../../services/gemini-powerbox-adv3-driver.md#hardware-validation):
a write probe over the serial port for the three protocol questions, and ConformU
through the driver for both devices, the Switch suites twice — once with both dew
channels in Manual, the state the unit was found in, and once with both in Auto, so each
dew row is found writable in the mode that allows it and read-only in the other.

Items 3 (the frame with a sensor unplugged) and 5 (output state across a hub power
cycle) needed hands on the unit and were answered on 2026-10-27 by two more probes
over the serial port, recorded [below](#items-3-and-5-2026-10-27).

The repository owner authorised the run with the ports as they were. ConformU
switched DC2–DC5 and USB A–F off and on, drove both dew duties across 0–100 % and
cycled both modes; the hub's total output current read 0.02–0.06 A before and
after the runs, so nothing drawing power hung on the outputs it switched.

## What was tested

| | |
|---|---|
| Commit | [`a647bae7`](https://github.com/rusty-photon/rusty-photon/commit/a647bae7) (branch `feature/gemini-powerbox-adv3-driver`, PR #1646), on `main` at `355ae19b`. The binary was built from that tree before its one later change, a rewording of a loader-check unit test for clippy; no line outside a test module differs |
| Service | `gemini-powerbox-adv3-driver`, default features, `cargo run -p gemini-powerbox-adv3-driver -- --config <config> -l debug` (dev profile); rustc 1.99.1 (58cf8f9c3 2026-10-13) |
| Config | `serial.port` `/dev/ttyUSB0`, `polling_interval` 5 s, `timeout` 2 s, `server.port` 11132 bound to 127.0.0.1, no TLS or auth, `averaging_period` 5 m |
| Probe | [`gemini_write_probe.py`](gemini_write_probe.py), run with the service stopped, before it; its output is [`write-probe.log`](write-probe.log). On 2026-10-27, with the tree at [`8b8941a2`](https://github.com/rusty-photon/rusty-photon/commit/8b8941a2) and no service running: [`gemini_sensor_probe.py`](gemini_sensor_probe.py), output [`sensor-probe.log`](sensor-probe.log), and [`gemini_power_probe.py`](gemini_power_probe.py), output [`power-probe.log`](power-probe.log); the kernel's USB log of that session is [`usb-journal.log`](usb-journal.log) |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.11-200.fc44, Linux `ch341` driver |
| Device | Gemini Astro Power & Data Hubs Advanced 3, `*HGeminiPowerBoxPlusAdv3#`, firmware `*V308#`; CH340 `1a86:7523` with no serial number, so the `UniqueID`s are the config's (`hw-smoke-switch`, `hw-smoke-weather`) |
| Sensors | AHT20 and DS18B20 both attached (frame flags `1`/`1`); 25.5 °C, 70 % RH, 19.7 °C dew point, lens 24.3 °C; 12.2 V in |
| ConformU | **4.5.3** build 56334.33d9e18 (the release GitHub named latest that day), against `http://127.0.0.1:11132/api/v1/{switch,observingconditions}/0`, settings file `{"SettingsCompatibilityVersion": 1}` — every delay and tolerance at ConformU's default |

Times below are PDT, 2026-10-26.

## Verdicts

| Device | State | Suite | Result |
|---|---|---|---|
| Switch | both channels Manual | `alpacaprotocol` | no errors, issues or information alerts ([log](alpacaprotocol-switch-manual.log)) |
| Switch | both channels Manual | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-switch-manual.log), [results](conformance-switch-manual-results.json)) |
| ObservingConditions | — | `alpacaprotocol` | no errors, issues or information alerts ([log](alpacaprotocol-observingconditions.log)) |
| ObservingConditions | — | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-observingconditions.log), [results](conformance-observingconditions-results.json)) |
| Switch | both channels Auto | `alpacaprotocol` | no errors, issues or information alerts ([log](alpacaprotocol-switch-auto.log)) |
| Switch | both channels Auto | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-switch-auto.log), [results](conformance-switch-auto-results.json)) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount` are all
**0** in the three results files. The Manual pass ran 22:29–22:44, the
ObservingConditions suites 22:44, the Auto pass 22:44–22:55. The Switch logs'
informational lines are ConformU's quarter-point writes (`Set: 0.25, Read: 0`,
`Set: 50.75, Read: 51`, …): the driver rounds a value between steps to the nearest
step, which ConformU reports and accepts. The ObservingConditions log's informational
lines list the properties the hub does not have (cloud cover, pressure, …) as absent
from `DeviceState`, which is right.

The dew rows moved their writability with the mode, as designed. In the Manual pass
ConformU found `DEW6 Duty` and `DEW7 Duty` writable (and drove them 0–100) and the
`Enabled` rows read-only, each `SetSwitch`/`SetSwitchValue` on them answering
`NotImplementedException`; in the Auto pass the reverse, the `Duty` rows reading `0`
— the loop's own output with the lens 4 °C above the dew point — and the `Enabled`
and `Mode` rows written and read back. Between the passes the modes were set to Auto
through the driver; after the Auto pass the enabled flags, the modes, the duties and
every output were written back, and the driver read the as-found state again: every
output `1`, both channels Manual, enabled, 100 %.

### The write path on hardware

The driver's log over both passes: **414 switch writes — ConformU's, and the
eighteen that set the modes to Auto between its passes and restored the as-found
state after them — all confirmed, none failed.** 30 of them (7 %) took the write
path's second look — the first frame read
after the set was a periodic frame that predated it, showing the old value — and the
second look confirmed every one. ConformU's own read-after-write checks saw the new
value each time. This is the rescue the design doc's
[write path](../../services/gemini-powerbox-adv3-driver.md#writes-are-confirmed-against-a-fresh-frame)
specifies, measured for the first time at scale; the probe below measured the
window it covers.

## The write probe

[`gemini_write_probe.py`](gemini_write_probe.py) opens the port as the driver does
(19200 8N1, DTR and RTS raised, `HUPCL` cleared), sends every set in **one buffer
with a `>G#` behind it**, exactly the request the driver composes, and logs every
chunk with a timestamp and every complete `*G` frame parsed. Sixteen sets, DEW7 and
the USB ports never touched, DC3 the one output toggled, and the end state the
as-found state. Its output is [`write-probe.log`](write-probe.log).

| Item | Question | Measured |
|---|---|---|
| 1 | Does `>C3#\n>G#\n` draw a frame showing DC3 changed? | Yes: `DC=1011` 124 ms after the send, and `>O3#\n>G#\n` drew `DC=1111` 124 ms after its send. The second command of a buffer is applied before the frame is composed. |
| 2 | Do fields 8/9 follow `>X`/`>Y` at once in Manual? | Yes: `>X50#` and `>X100#` each showed in field 8 in the frame behind them. |
| 2 | What does field 8 show in Auto? | The loop's live duty. After `>M10#` it kept `100` for some twenty seconds, then a `>X30#` sent in Auto showed `30` for one frame and `0` three seconds later — the loop's output with the lens 4 °C above the dew point. `>M11#` back to Manual **kept `0`**: one register per channel, written by the loop and by `>X` alike. |
| 4 | Is `>M1x#` reflected in field 5, `>Z1x#` in field 4? | Yes, every one in the frame behind it: `>M12#` → `2`, `>M10#` → `0`, `>M11#` → `1`; `>Z10#`/`>Z11#` → `0`/`1` in Switch, Auto and Manual alike. In Manual the flag changed and the duty field did not; what the heater did was not observed, and the driver refuses the write in Manual regardless. |

Timing: a bare `>G#` drew its whole 81-byte frame 49 ms after the send (twice); a set
with its `>G#` drew the frame 123–125 ms after the send (every set), which with the
frame's 42 ms on the wire puts the hub's first byte about 80 ms after a set.

**The pre-set periodic frame.** Three of the sixteen sets — `>X100#` at 11.023 s,
`>Z10#` at 50.028 s, `>Z11#` at 56.029 s — were followed first by a periodic frame
showing the state *before* the set, then 85 ms later by the reply proper showing it
applied. The periodic stream ran at a fixed phase (frames ending at x.132 s every 3 s);
each of those sets went out 104–109 ms before that frame's last byte, so the frame's
first byte landed some 65 ms *after* the set, and still predated it. The window is
therefore about 110 ms per 3 s period, roughly one write in twenty-seven, against the
5 ms the design doc had estimated; the doc now carries the measurement. The driver's
second look is what covers it, and the ConformU passes above are the proof.

## Items 3 and 5, 2026-10-27

Both probes open the port as the write probe does and log every chunk with a
timestamp. Neither involves the driver: the facts are the hub's. Times are PDT.

### Item 3: the frame with a sensor unplugged

[`gemini_sensor_probe.py`](gemini_sensor_probe.py) sends a bare `>G#` every two
seconds and prints every frame raw, then split: the fixed fields 0–9, the sensor
and electrical remainder exactly as written, and whether the driver's shape (each
sensor field ending at its own tag) still matches. A label file names the phase on
every line. The operator pulled and replugged the sensors between 17:03 and 17:11;
the output is [`sensor-probe.log`](sensor-probe.log).

| Step | State | Flags | Sensor fields as written | Driver shape |
|---|---|---|---|---|
| 0 | both attached | `U1A1T` | ` 24.06S 25.17T 71.20H 19.58D` | matches |
| 1 | AHT20 out | `U0A1T` | ` 24.06S0T0H0D` | matches |
| 2 | DS18B20 out too: both out | `U0A0T` | `0S0T0H0D` | matches |
| 3a | AHT20 back in, DS18B20 still out | `U1A0T` | `0S 25.13T 71.55H 19.63D` | matches |
| 3b | AHT20 out again: both out | `U0A0T` | `0S0T0H0D` | matches |
| 3c | DS18B20 back in, AHT20 out | `U0A1T` | ` 24.38S0T0H0D` | matches |
| 4 | AHT20 back in: both attached | `U1A1T` | ` 24.38S 25.07T 71.63H 19.59D` | matches |

Step 3 was asked for as one move — the probe back in, the AHT20 left out — and the
log shows the operator made three: the AHT20 in at 87 s of the second run, out again
at 102 s, then the probe in. The table follows the log.

**The firmware writes a bare `0`** — one character, no padding — into each field
the missing sensor would fill, and drops the flag in the same frame; nothing else
in the frame changes, no `*`, no letter, no blank. A replugged sensor comes back
the same way, flag and readings together. How long the firmware takes to notice a
pull was not timed: the operator waited a few seconds before answering, and the
first frame read after each answer already showed the change.

**The DS18B20's 3.5 mm jack resets the hub's USB side.** Pulling it (17:05:57) and
inserting it (17:08:21) each dropped the hub's whole USB tree — its VIA Labs hub,
the internal `1a86:8091` hub, the CH340 serial link and a VIA Labs billboard
device — which re-enumerated about three seconds later with `ttyUSB0` back under
the same name ([`usb-journal.log`](usb-journal.log)); the probe's next write failed
with `EIO`, which is the two tracebacks in the sensor log, and it was restarted
each time. The AHT20's connector did not: it moved four times (steps 1, 3a, 3b and
4) and the probe ran through every one without losing the port. The operator
confirmed only the probe's plug moved. Two of two for the jack, zero of four for the
AHT20's connector.

### Item 5: output state across a power cycle

[`gemini_power_probe.py`](gemini_power_probe.py) runs in three phases, each its own
process because the hub's CH340 dies with its 12 V; the output is
[`power-probe.log`](power-probe.log).

| Phase | Time | What happened |
|---|---|---|
| `before` | 17:11 | Baseline every output on, both channels Manual, enabled, 100 %. Set, each in one buffer with a `>G#` behind it: DC3 off, USB A off, DEW6 duty 50, DEW6 mode Switch, DEW6 disabled. Frame before the cut: `DC=1011 USB=011111 DEW6 en=0 mode=2 duty=50`, DEW7 untouched |
| the cut | 17:13:20–17:13:34 | The operator cut the hub's 12 V input; the kernel saw the USB tree go at 17:13:20 and come back at 17:13:34, `ttyUSB0` attached at 17:13:36 — 14 s without power |
| `after` | 17:13 | Waited for the port to vanish and return, then listened 12 s without sending: **nothing arrived** — the hub does not stream until asked, after a power-up as after an open. `>H#` → `*HGeminiPowerBoxPlusAdv3#`, `>V#` → `*V308#`, `>G#` → `DC=1011 USB=011111 DEW6 en=0 mode=2 duty=50`: **every setting as before the cut** |
| `restore` | 17:14 | DC3 on, USB A on, DEW6 Manual, enabled, 100 %, each confirmed in the frame behind it; final frame the state the unit was found in on 2026-10-26 |

**The hub keeps its outputs, its USB ports and each dew channel's mode, flag and
duty across a power cycle.** One cycle of 14 s on one unit; the USB chips were
down for all of it, so this is stored state, not hold-up. The design doc's earlier
reading of the capture — that a heater runs at full power from power-up — holds
only for a unit whose channels were left at 100 % Manual, which is how this unit
was found and how AlpacaBridge reports its own came up.

## Privacy scrub

The 2026-10-26 logs name only `127.0.0.1:11132` and the operating system; no
hostname, address or user name appears in them. The 2026-10-27 probe logs name
only `/dev/ttyUSB0` and its `by-id` link, except the two Python tracebacks in
[`sensor-probe.log`](sensor-probe.log), whose script path under the operator's
home directory is replaced by `<probe-dir>`. [`usb-journal.log`](usb-journal.log)
is the kernel log of 17:05–17:15 filtered to the USB lines, with the machine's
hostname replaced by `<dev-box>` on every line; nothing else in either was touched.

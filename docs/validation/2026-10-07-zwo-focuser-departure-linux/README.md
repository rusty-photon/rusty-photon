# zwo-focuser on the field rig: an EAF that leaves the bus, 2026-10-07

This record covers issue #1431: a ZWO EAF that loses its cable or its power
while connected must read disconnected. It is zwo-focuser's first
validation record, and it has three parts:

- **What the EAF SDK does when an open EAF leaves.** This was measured
  before the contract was written, and it differs from the ASI cameras'
  SDK, which hides a departure until something rescans
  ([zwo-camera's record](../2026-10-06-zwo-camera-departure-linux/README.md)).
- **The new C5 on hardware.** Five departure phases against the physical
  EAF, through the service's Alpaca API.
- **A ConformU run of the same build**, since the change reworked the
  connect path: every open now rescans and finds the EAF by its serial.

The departures are real USB disconnects. The EAF hangs off a two-port USB2
hub on the rig, next to the Scops OAG's FTDI adapter. Writing `1` to that hub
port's sysfs `disable` file takes the EAF off the bus (the kernel logs a USB
disconnect and `/dev/hidraw*` goes away) while the EAF keeps its power, and
writing `0` brings it back. No probe touched the neighbouring port.

**Nothing moved the focuser except ConformU.** The SDK probe and the departure
phases read, connect, disconnect and halt (a stop-class call); the one
`EAFMove` the SDK probe sends targets the position it last read, and only
while sysfs says the EAF is off the bus. ConformU's move test drove the
focuser 6000 steps out and back, and left it where it started (34637).

## What was tested

| | |
|---|---|
| Commit | [`6eb0a5ef`](https://github.com/rusty-photon/rusty-photon/commit/6eb0a5ef) (branch `fix/zwo-focuser-eaf-left-bus-1431`), on `main` at `7bf0d821`. The rig's build tree was checked against it: identical tree hash (`8a87e685`). |
| Service | `zwo-focuser`, **real-SDK** build (default features), `cargo run -p zwo-focuser -- --config … --log-level debug` (dev profile) on the rig, run as the `rusty-photon` user so it can open `/dev/hidraw*`, served on `127.0.0.1:11134` over plain HTTP with no auth. The packaged service was stopped for the run and restarted after it. rustc 1.99.0 (b940084d7 2026-09-28) |
| SDK | EAF SDK **1.7.7** (`EAFGetSDKVersion` → `1, 7, 7, 0`): `/usr/lib/rusty-photon/libEAFFocuser.so` from the installed `rusty-photon-zwo-focuser` package (`0.1.0+nightly.202610061157.g7700ee9`), sha256 `0ce25b248a727ec4cad07cc4de4ffeb256773f9bc23794a66a8a527132d5df6d` |
| Platform | Raspberry Pi 5 Model B Rev 1.1, Raspberry Pi OS (Debian GNU/Linux 13, trixie) aarch64, kernel 6.18.34+rpt-rpi-2712 — the field rig |
| Device | ZWO EAF, firmware 3.8.1, `ZWO:EAF:6000e072a1e28f94`, working travel limit (`EAFGetMaxStep`) 60000, at step 34637 throughout |
| Departure | sysfs `disable` on the EAF's port of its USB2 hub, as root |
| ConformU | **4.5.0** build 53834.49ab847 (the current upstream release), against `http://127.0.0.1:11134/api/v1/focuser/0` with a settings file of `{"SettingsCompatibilityVersion": 1}` |

## Verdicts

| Suite | Result |
|---|---|
| `alpacaprotocol` | *"no errors, issues or information alerts"* ([log](alpacaprotocol.log)) |
| `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance.log), [results](conformance-results.json)) |

`ErrorCount`, `IssueCount`, `ConfigurationAlertCount` and `TimingIssuesCount`
are all **0** in the results file, over 20 timings. The suites ran from a
fresh instance of the same build after the departure phases, so their
connects went through the new open: a rescan, then a match by serial. The
service logged no departure and no presence check during either suite.

## What the SDK does when an EAF leaves

A probe of the SDK itself, written with Python's `ctypes` against the
packaged library and run as root, opened the EAF, read every member it
could, disabled the port, and asked again at each stage
([departure](sdk-departure-probe.txt), [blink](sdk-blink-probe.txt)):

| State of an EAF opened before it left | What the SDK answers |
|---|---|
| Gone, no rescan yet (+2 s) | Every call that needs the session answers **`EAF_ERROR_REMOVED`** at once (0.0 ms): `EAFGetPosition`, `EAFIsMoving`, `EAFGetTemp`, `EAFGetMaxStep`, `EAFGetReverse`, `EAFGetBacklash`, `EAFGetSerialNumber`, `EAFGetFirmwareVersion`, `EAFStop`, `EAFMove`. `EAFGetProperty`, which needs no session, still answers from memory. |
| Gone, after `EAFGetNum` | The rescan (10 ms) lists no EAF. Every call on the old ID answers `INVALID_ID`, and so does `EAFOpen` on it. |
| Back, no rescan since (the departure case) | Every call on the old ID still answers `INVALID_ID`. |
| Back, after a rescan (the departure case) | The EAF is listed again under the **same** ID 0, now a fresh, unopened entry: the old session's calls answer `EAF_ERROR_CLOSED`, and `EAFGetProperty` succeeds. Closing the old ID and opening the listed one works. |
| Off for 3 s and back with no SDK call in between (the blink case) | The old session does not reattach: every call answers `REMOVED`. A rescan then lists the EAF under a **new** ID (1); the old ID answers `INVALID_ID`, even to `EAFClose`. Opening the listed ID works. |
| Present, rescans running beside it | ID and session unchanged; every call succeeds. A rescan takes 1–4 ms. |

The SDK's own log (`/tmp/zwo/log/eaf_sdk/`, which it writes by itself) shows
the mechanism: from the departure on, each call fails with
`HID Err: ioctl (SFEATURE): No such device`, which the SDK reports as
`EAF_ERROR_REMOVED`. A rescan after a blink binds the new ID to the new hidraw
node (`/dev/hidraw0` → `/dev/hidraw1`).

C5 is built on that table. `REMOVED`, `INVALID_ID` and `CLOSED` on an open
session mean the EAF has left, and mark the session lost. Any other failure
asks once more with a session read on the same ID. Every open rescans and
finds the EAF by its serial (design doc
[C5](../../services/zwo-focuser.md#enumeration--connection-lifecycle)).

## The new C5 on hardware

Five phases, run in order against one service instance
([transcript](departure-probes.txt)). Times are local (UTC−7); the service
log is UTC.

### 1. Idle: only a failure asks

```
08:49:09.929  sysfs port disable <- 1
08:49:11.930  GET connected                value=True     err=0x0   (1.2 ms)
08:49:11.931  GET maxstep                  value=60000    err=0x0   (0.9 ms)
08:49:11.932  GET position                 value=None     err=0x407 NOT_CONNECTED  (1.1 ms)
08:49:11.933  GET connected                value=False    err=0x0   (0.9 ms)
08:49:11.934  GET ismoving                 value=None     err=0x407 NOT_CONNECTED  (0.8 ms)
08:49:11.935  GET temperature              value=None     err=0x407 NOT_CONNECTED  (0.8 ms)
08:49:11.936  GET maxstep                  value=None     err=0x407 NOT_CONNECTED  (0.8 ms)
08:49:11.937  GET maxincrement             value=None     err=0x407 NOT_CONNECTED  (0.8 ms)
08:49:11.938  PUT halt                     value=None     err=0x407 NOT_CONNECTED  (0.9 ms)
```

```
15:49:11.932515Z  WARN zwo_focuser::backend: the focuser has left the bus; it reads disconnected
                  until a client releases it focuser=ZWO:EAF:6000e072a1e28f94
                  failure=EAF focuser SDK error: focuser removed
```

Two seconds after the departure, `Connected` and the cache-served `MaxStep`
still answered: nothing had reached the SDK. The first `Position` did, met
`REMOVED`, and answered `NOT_CONNECTED` itself. From then on the device read
disconnected, and every member that needs a session, `MaxStep` and
`MaxIncrement` included, answered `NOT_CONNECTED`.

### 2. A reconnect while the EAF is still gone

`Connected = true` released the lost session and then failed with
`NOT_CONNECTED` in 10.9 ms: the open's rescan found no EAF. The device stayed
disconnected.

### 3. Back: a plain reconnect finds the EAF by its serial

```
08:49:14.589  PUT connected(Connected=True) value=None     err=0x0   (135.9 ms)
08:49:14.594  GET position                 value=34637    err=0x0   (3.5 ms)
08:49:14.602  PUT halt                     value=None     err=0x0   (4.3 ms)
```

### 4. Off and on again, with nothing failing in between

The port was disabled for 3 s and re-enabled with no client call in between,
so the session was held across the whole blink.

```
08:49:20.331  GET connected                value=True     err=0x0   (1.6 ms)
08:49:20.333  GET position                 value=None     err=0x407 NOT_CONNECTED  (1.6 ms)
08:49:20.334  GET connected                value=False    err=0x0   (1.2 ms)
08:49:20.473  PUT connected(Connected=True) value=None     err=0x0   (139.5 ms)
08:49:20.477  GET position                 value=34637    err=0x0   (3.2 ms)
```

The held session answered `REMOVED` although the EAF was back, and read
disconnected after it. `Connected = true`, with no `Connected = false`
before it, released the lost session and connected afresh. The rescan in
that open listed the EAF under a new ID, and the match by serial found it.

### 5. A lost session disconnects cleanly

`Temperature` found the departure, and `Connected = false` released the lost
session and succeeded (11.5 ms). Once the EAF was back, a plain connect
worked.

## Things the run taught us

- **The EAF SDK aborts its process when it cannot write its own log.** It
  logs to `/tmp/zwo/log/eaf_sdk/` through spdlog, and when that directory is
  not writable the exception escapes the C API:
  `terminate called after throwing an instance of 'spdlog::spdlog_ex'` …
  `Failed opening file /tmp/zwo/log/eaf_sdk/… Permission denied`, at the
  first SDK call. It happened here because the root-run SDK probe had created
  `/tmp/zwo` first. The packaged unit is immune: it runs with
  `PrivateTmp=yes`. Anything else that loads the SDK as another user on a
  shared `/tmp`, a hand-run service or the doctor subcommand, dies the same
  way. Removing the root-owned directory cleared it.

## Still owed

- **Windows.** Every measurement here is Linux, EAF SDK 1.7.7.
- **A cable pull or a power cut.** These departures disabled the hub port:
  the kernel saw a disconnect, but the EAF kept its power.
- **A departure mid-move.** No probe moved the focuser, so what the motor and
  the SDK do when the EAF leaves during a move is unmeasured.

## Files

- [`departure-probes.txt`](departure-probes.txt): the five phases above, as
  the client printed them.
- [`sdk-departure-probe.txt`](sdk-departure-probe.txt),
  [`sdk-blink-probe.txt`](sdk-blink-probe.txt): the raw SDK probes.
- [`alpacaprotocol.log`](alpacaprotocol.log), [`conformance.log`](conformance.log),
  [`conformance-results.json`](conformance-results.json): ConformU's output,
  unmodified.

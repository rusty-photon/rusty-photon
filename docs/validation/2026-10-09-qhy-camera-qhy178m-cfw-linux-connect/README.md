# qhy-camera on Linux: QHY178M + CFW, 2026-10-09 (a connect owns the device)

This is a recorded Linux ConformU run against the same physical QHY178M + CFW as the
[2026-10-05 record](../2026-10-05-qhy-camera-qhy178m-cfw-linux-departure/README.md).
It was taken on the change that makes a connect own the device across its handshake
and hold the cooler still while it runs
([C6, RM4](../../services/qhy-camera.md#enumeration--connection-lifecycle), PR #1452).

Next to the ConformU run are the probes the change exists for:

- what a client sees while a connect is running: `CameraState` and `StartExposure`;
- a `CoolerOn` sent into a running connect, with the SDK's default settings;
- the same `CoolerOn`, with `disable_auto_cooler=true`, against this branch and
  against `main` — where `main` loses the cooler on real hardware and the branch
  does not.

## What was tested

| | |
|---|---|
| Commit | [`9214df9b`](https://github.com/rusty-photon/rusty-photon/commit/9214df9b) (branch `fix/qhy-camera-connect-owns-device`, PR #1452), on `main` at `7fdbf4e6` |
| Control | `main`'s `services/qhy-camera/src/camera.rs` at `7fdbf4e6`, built in the same tree — the branch changes no other source file |
| Service | `qhy-camera`, **real-SDK** build (default features), `cargo run -p qhy-camera -- --log-level debug` (dev profile); rustc 1.99.0 (b940084d7 2026-09-28) |
| SDK | QHYCCD SDK **26.06.04**: `/usr/local/lib/libqhyccd.so` → `libqhyccd.so.26.6.4.16`, sha256 `f51b92f9189fae7707e98ad334cf52d3c1493a6485f33394b39a18a3f4d5c738` (byte-identical to every earlier QHY record) |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.8-200.fc44 |
| Camera | QHY178M-Cool (USB `1618:c179`, SuperSpeed), SDK id `QHY178M-222b16468c5966524`, with its 7-slot CFW. It sits on USB3 port 3 of the Pegasus PPBA Gen2's embedded hub, and its 12 V comes from the PPBA's Quad 12V output, driven through `ppba-driver` (`cargo run -p ppba-driver`) |
| ConformU | **4.5.0** build 53834.49ab847 (the latest release), against `http://127.0.0.1:11121/api/v1/{camera,filterwheel}/0` |

The Quad 12V output was switched on before the service started. The CFW still
enumerated late (`cameras=1 filter_wheels=0`); a reload (SIGHUP) straight away still
found none, and a second one about 30 s after power-on found it (`filter_wheels=1`).
C0 describes the same behaviour. Three 10 ms light frames were taken before ConformU
started, each ready in 2.6 s.

## Verdicts

| Device | Suite | Result |
|---|---|---|
| Camera | `alpacaprotocol` | 0 errors, 0 issues, 16 information messages ([log](alpacaprotocol-camera.log)) |
| Camera | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-camera.log), [results](conformance-camera-results.json)) |
| FilterWheel | `alpacaprotocol` | no errors, issues or information alerts ([log](alpacaprotocol-filterwheel.log)) |
| FilterWheel | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-filterwheel.log), [results](conformance-filterwheel-results.json)) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are all **0** in both results files. The 16 informational items are the set every QHY
record carries. Every connect in both suites — `alpacaprotocol` alone drives several,
in every casing — ran through the new path, and the service logged no `WARN` or
`ERROR` of its own.

## The probes

The probes are Alpaca requests made with a small client. Times are seconds from the
start of each probe. Each probe asserted its preconditions first: the Quad 12V output
on (read back from the PPBA's switch), `1618:c179` on USB, and both devices in the
service's configured-device list.

### 1. While a connect runs (C6, B4, E2)

The camera was disconnected; a `Connected = true` was sent, and `CameraState` was
polled as fast as the client could until it returned. The first `CameraState` to
answer triggered one `StartExposure`.

```
PUT connected                 0.025 → 0.329 s   err=0x0
GET camerastate (during it)   NOT_CONNECTED ×2 (before the open), Exposing ×1212, Idle ×0
PUT startexposure  at 0.026   err=0x40B INVALID_OPERATION
                              "the device is busy: an exposure is in flight, or the
                               camera is connecting, changing readout mode or disconnecting"
GET camerastate (after it)    Idle
```

From the open until the connect returned, the device reported itself busy and
refused an exposure as busy, the answers a readout-mode change already gives (B4).
This side was not run against `main`; there the unit tests this PR rewrote pinned
`Idle` and `INVALID_VALUE` (the exposure range not yet published) for the same
window.

### 2. `CoolerOn` into a running connect, SDK defaults

The target (0 °C) was set in a session of its own, with the cooler off, and the
camera was disconnected; the target is the last command given and outlives the
reconnect (K4). A `Connected = true` was then sent, `Connected` polled, and
`CoolerOn = true` sent the moment it first read true — inside the handshake.

```
PUT connected    0.145 → 0.448 s
PUT cooleron     0.146 → 0.448 s   (returned with the connect, not before it)
TEC after it     power 0 → 50.6 % by 3.5 s; sensor 24.2 → 9.4 °C over 20 s; CoolerOn true
```

### 3. `disable_auto_cooler=true`: what a reconnect's init does to the TEC

The SDK reads `qhyccd.ini` from the service process's **working directory**: it
logs `Load ini filePath = <that directory>  fileName = qhyccd.ini` at startup. For
sections 3 and 4, a copy of the SDK's sample `qhyccd.ini` with only
`disable_auto_cooler` flipped to `true` was placed there, and removed afterwards.

```
PUT cooleron     (target 0 °C)
TEC engaged      power 0 → 100 % by 3 s; sensor 13.1 → 1.4 °C over 15 s
PUT connected false, then true      (a plain reconnect, nothing in between)
TEC after it     power 0 % for 15 s; sensor 0.9 → 10.7 °C; CoolerOn true
```

K4 says a reconnect on such a rig leaves the TEC off beside a `CoolerOn` that still
reads true, from a reading of the SDK library. This is that statement measured. It
is unchanged by this PR: the connect re-asserts nothing (C5).

### 4. The race, `main` against this branch (`disable_auto_cooler=true`)

Section 2's probe, three times on each build.

| Build | Run | `Connected` PUT (s) | `CoolerOn` PUT (s) | Returned before the connect | TEC power after | Sensor over 20 s | `CoolerOn` |
|---|---|---|---|---|---|---|---|
| `main` | 1 | 1.311 → 1.617 | 1.312 → 1.313 | **yes** | **0 %** throughout | 14.1 → 18.0 °C | true |
| `main` | 2 | 0.460 → 0.764 | 0.461 → 0.462 | **yes** | **0 %** throughout | 19.3 → 20.8 °C | true |
| `main` | 3 | 0.871 → 1.175 | 0.872 → 0.872 | **yes** | **0 %** throughout | 21.0 → 21.9 °C | true |
| branch | 1 | 0.248 → 0.555 | 0.250 → 0.555 | no | peak 100 % | 14.7 → 0.8 °C | true |
| branch | 2 | 1.311 → 1.614 | 1.312 → 1.614 | no | peak 100 % | 22.6 → 2.5 °C | true |
| branch | 3 | 0.888 → 1.192 | 0.889 → 1.192 | no | peak 100 % | 2.0 → −1.4 °C | true |

On `main` the `CoolerOn` reached the camera in about a millisecond, mid-handshake,
and the rest of the handshake then switched the TEC off: `CoolerOn` read true while
the sensor warmed, 3 out of 3. The write landed after the open, with no close after
it, so what stops the TEC is in the handshake rather than the close — its
`InitQHYCCD`, by the reading of the SDK library in RM4. On the branch the same
request waited for the connect and returned with it, and the TEC regulated, 3 out of
3.

## Before this run

An attempt the evening before (2026-10-08), on the same production code, wedged the
camera on its first exposure: `StartExposure` was accepted and the capture never
returned from the SDK, so every later exposure test was abandoned and the wheel could
not connect. A service restart and a Quad 12V cycle left it enumerating as
`cameras=0` with `1618:c179` still on the bus — the #755 signature. Re-seating the
USB cable cleared it, and this run followed. The hang was the first exposure since
that service had started, so whether the camera was already wedged before it is
unknown; the connect before it had completed and handed the device back.

## After the run

The cooler was switched off, both services stopped, the test `qhyccd.ini` removed,
and the Quad 12V output turned off again, the box's between-sessions state.

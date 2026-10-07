# zwo-camera on Linux: a camera that leaves the bus, 2026-10-06

This record covers issue #1411 (PR #1417): a ZWO camera that loses its power or
its cable while connected must read disconnected. It has three parts:

- **What the ASI SDK does when a camera leaves.** This was measured first, and
  it overturned the change's first design.
- **The redesigned C6 on hardware.** Five departure probes against three
  physical cameras.
- **A ConformU run of the same build on all three cameras**, since the change
  reworked the connect path.

The departures are real: the cameras left the USB bus. Two of them, an
ASI120MC-S and an ASI178MM, hang off a USB2 hub on the Pegasus PPBA's
switchable USB2 port. The PPBA's `USB Hub` switch (`PU`), driven through
`ppba-driver`, cuts that port's power, and both cameras vanish from `lsusb`.
The ASI1600MM-Cool sits on the PPBA hub's USB3 side, which `PU` does not
switch, and serves as the control throughout.

## What was tested

| | |
|---|---|
| Commit | [`efb54246`](https://github.com/rusty-photon/rusty-photon/commit/efb54246) (branch `fix/zwo-camera-camera-left-bus-1411`, PR #1417), on `main` at `c1ffe919` |
| Service | `zwo-camera`, **real-SDK** build (default features), `cargo run -p zwo-camera -- --log-level debug` (dev profile); rustc 1.99.0 (b940084d7 2026-09-28) |
| SDK | ASI SDK **1.41** (`ASIGetSDKVersion` → `1, 41, 0, 0`): `/usr/local/lib/libASICamera2.so`, sha256 `d1de4a5ab85c8cafbddfad9c593bbba515890d3adf20c1ca44dafcf15f2775ce` (the same file as the [2026-10-01 record](../2026-10-01-zwo-camera-asi1600mm-cool-gain-offset-linux/README.md)) |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.8-200.fc44 |
| Cameras | device 0 `ZWO:ZWO-ASI1600MM-Cool:noserial-0` (USB3, the control); device 1 `ZWO:ZWO-ASI120MC-S:1f19470620070900`; device 2 `ZWO:ZWO-ASI178MM:1915d5081b090900` (both on the switched USB2 port) |
| Departure | PPBA Gen2C switch 4 (`USB Hub`) through `cargo run -p ppba-driver` on loopback. The PPBA's own serial link survives the toggle and was checked after every one. Quad 12V stayed off throughout. |
| ConformU | **4.5.0** build 53834.49ab847 (the current upstream release), against `http://127.0.0.1:11122/api/v1/camera/{0,1,2}` |

## Verdicts

| Camera | `alpacaprotocol` | `conformance` |
|---|---|---|
| ASI1600MM-Cool (device 0) | no errors, issues or information alerts ([log](asi1600mm-cool-alpacaprotocol.log)) | *"no errors, warnings or issues found"*; every member within its target response time ([log](asi1600mm-cool-conformance.log), [results](asi1600mm-cool-conformance-results.json)) |
| ASI120MC-S (device 1) | no errors, issues or information alerts ([log](asi120mc-s-alpacaprotocol.log)) | *"no errors, warnings or issues found"*; every member within its target response time ([log](asi120mc-s-conformance.log), [results](asi120mc-s-conformance-results.json)) |
| ASI178MM (device 2) | no errors, issues or information alerts ([log](asi178mm-alpacaprotocol.log)) | *"no errors, warnings or issues found"*; every member within its target response time ([log](asi178mm-conformance.log), [results](asi178mm-conformance-results.json)) |

`ErrorCount`, `IssueCount`, `ConfigurationAlertCount` and `TimingIssuesCount` are all
**0** in all three results files. The suites ran after the departure probes, from the
same service instance, so every connect in them went through the redesigned open:
a rescan, then a match by name and serial. Across the three `conformance` runs the
service logged no departure, and no presence check that answered neither way.

## What the SDK does when a camera leaves

C6 was first built on the SDK header's description of `ASI_ERROR_CAMERA_REMOVED`
(*"failed to find the camera, maybe the camera has been removed"*). That build
(`5f234687`) was put on this hardware first, and its departure never
registered ([transcript](previous-build-departure.txt)). With both cameras
gone from `lsusb`, `CCDTemperature` kept answering its last value for 20 s.
Each camera's next exposure then failed in its arm with `GENERAL_ERROR`, and
a 10 s exposure ended `ASI_EXP_FAILED`. `Connected` stayed `true` throughout,
and the session kept failing even after the cameras came back.

A probe of the SDK itself, written with `ctypes` against the same library, then
asked each call what it answers
([departure](sdk-departure-probe.txt),
[rescans beside open cameras](sdk-rescan-beside-open-cameras.txt),
[rescans across a departure](sdk-rescan-across-departure.txt)):

| State of a camera opened before it left | What the SDK answers |
|---|---|
| Gone, no rescan yet | `ASIGetControlValue` (temperature, gain) and `ASIGetCameraPropertyByID` succeed with the old values. `ASISetROIFormat` and `ASIStartExposure` succeed. `ASISetControlValue` fails `GENERAL_ERROR`. The exposure status turns `ASI_EXP_FAILED` about 1.2 s after the start. **No call answers `CAMERA_REMOVED`.** |
| Gone, after `ASIGetNumOfConnectedCameras` | The rescan (about 320 ms after a change, 18 ms otherwise) lists only the cameras still there. Every call on the departed camera's ID answers `INVALID_ID`, and so does `ASIOpenCamera` on it. |
| Back, no rescan since | Every call on the old ID still answers `INVALID_ID`. |
| Back, after a rescan | The camera is listed afresh. `ASIGetCameraPropertyByID` on the old ID answers `CAMERA_CLOSED`. Closing the old ID and opening the listed one works. |
| Present, rescans running beside it | IDs unchanged. An exposure integrating through two rescans completed, and later exposures were unaffected. |

The redesign is built on that table. Every SDK failure now asks whether the
camera is still there: a rescan, then `ASIGetCameraPropertyByID` on its ID
(`zwo_rs::Sdk::still_connected`). `INVALID_ID` or `CAMERA_CLOSED` means the
camera is gone. Every open rescans and finds its camera by name and serial
(design doc [C6](../../services/zwo-camera.md#enumeration--connection-lifecycle)).

The raw probe once saw `ASIStopExposure` block for 80 s on a freshly reopened
camera. That window (17:47:56–17:49:16) is exactly when a QHY178M on the same
VL813 hub was wedged in 85 s of USB descriptor timeouts. It did not recur in an
identical run once the QHY was quiet, so it is hub contention, not ASI SDK
behaviour.

## The redesign on hardware

Five phases, run in order against the one service instance
([transcript](departure-probes.txt)). Times are local (UTC−7); the service log
is UTC.

### 1. Idle: only a failure asks

```
--- reads on a hidden departure: answered, and nothing asks ---
23:15:14  GET 1/ccdtemperature               value=28.2         err=0x0
23:15:14  GET 1/gain                         value=100          err=0x0
23:15:14  GET 1/connected                    value=True         err=0x0
--- the first failure: an exposure's arm ---
23:15:14  PUT 1/startexposure                value=None         err=0x0
23:15:15  GET 1/connected                    value=False        err=0x0
23:15:15  GET 1/camerastate                  value=None         err=0x407 NOT_CONNECTED
23:15:15  GET 1/gain                         value=None         err=0x407 NOT_CONNECTED
23:15:15  PUT 2/startexposure                value=None         err=0x0
23:15:17  GET 2/connected                    value=False        err=0x0
```

```
06:15:14.585012Z WARN zwo_camera::backend: the camera has left the bus; it reads disconnected until a
                 client releases it camera=ZWO:ZWO-ASI120MC-S:1f19470620070900
                 failure=failed to set gain: ASI camera SDK error: general error (e.g. value out of valid range)
06:15:15.786961Z WARN zwo_camera::backend: the camera has left the bus; ...
                 camera=ZWO:ZWO-ASI178MM:1915d5081b090900 failure=ASI camera SDK error: invalid camera ID
```

The ASI120MC-S's arm failed `GENERAL_ERROR`, and the check's rescan found it
gone. That rescan also dropped the ASI178MM from the SDK's list, so its arm
failed `INVALID_ID`, and its own check confirmed. `Connected = false` then
released each session, and `Connected = true` failed with `NOT_CONNECTED` while
the cameras were still gone. The control camera took a frame.

### 2. Back: a plain reconnect finds each camera by its identity

```
23:15:32  PUT 1/connected (true)             err=0x0  (755.3 ms)
23:15:32  GET 1/gainmax                      value=100    (the ASI120MC-S's own)
23:15:33  GET 1/imageready                   value=True
23:15:33  PUT 2/connected (true)             err=0x0  (351.9 ms)
23:15:33  GET 2/gainmax                      value=510    (the ASI178MM's own)
23:15:35  GET 2/imageready                   value=True
```

Rescans had renumbered the SDK's list in between. Each open matched its camera
by name and serial, and `GainMax` shows each device on its own body.

### 3. A departure 3 s into a 10 s exposure

```
23:15:43  PUT 2/startexposure (10 s)         err=0x0
23:15:46  PPBA setswitch USB Hub=False
23:15:52  t+  9.1s 2/connected               value=True
23:15:54  t+ 11.1s 2/connected               value=False
```

The watcher read only `Connected`, which never reaches the SDK. The capture's
readout poll found the exposure failed, asked, and marked the session lost with
no client call (`failure=exposure failed` in the service log). The idle ASI120MC-S
still read connected, as C6 says: nothing on it had failed yet.

### 4. Back, with a session held across the departure

The ASI120MC-S's session had been open since phase 2. Its next exposure failed
`INVALID_ID`, and the check found the old ID no longer naming a listed camera.
It read disconnected, and a reconnect brought both devices back on their own
bodies.

### 5. Off and on again, with nothing failing in between

This is the case that never healed on the previous build. Both sessions were
fresh, and the USB2 port was switched off for 3 s and back on with no SDK call
in between, so no rescan ran while the cameras were gone.

```
23:16:29  GET 1/connected                    value=True
23:16:29  PUT 1/startexposure                err=0x0
23:16:29  GET 1/camerastate                  value=None  err=0x407 NOT_CONNECTED  (319.9 ms)
23:16:29  GET 1/connected                    value=False
23:16:29  PUT 2/startexposure                err=0x0
23:16:29  GET 2/camerastate                  value=None  err=0x407 NOT_CONNECTED
23:16:30  GET 2/connected                    value=False
```

The ASI120MC-S's arm failed `GENERAL_ERROR`. The check's rescan listed the
returned camera afresh, so its old ID answered `CAMERA_CLOSED`, and the session
ended. The ASI178MM's arm then met `CAMERA_CLOSED` directly. Both reconnected
onto their own bodies and took frames. The cost is one failed exposure per
camera. On the previous build it was every exposure until a client
reconnected.

The control camera, on USB3, kept its session and took a frame after every
phase.

## Still owed

- **Windows.** Every measurement here is Linux with SDK 1.41.
- **A blank frame on ASI hardware.** C6 discards an all-zero frame from a
  departed camera, because a QHY readout returned one on Windows. No ASI
  readout here was seen to; the departures surfaced as `ASI_EXP_FAILED` before
  any download.

## Files

- [`departure-probes.txt`](departure-probes.txt): the five phases above, as the
  client printed them.
- [`previous-build-departure.txt`](previous-build-departure.txt): the same kind
  of departure on build `5f234687`.
- [`sdk-departure-probe.txt`](sdk-departure-probe.txt),
  [`sdk-rescan-beside-open-cameras.txt`](sdk-rescan-beside-open-cameras.txt),
  [`sdk-rescan-across-departure.txt`](sdk-rescan-across-departure.txt): the raw
  SDK probes.
- `asi1600mm-cool-*`, `asi120mc-s-*`, `asi178mm-*`: ConformU's
  `alpacaprotocol` and `conformance` logs, and the `conformance` results, per
  camera.

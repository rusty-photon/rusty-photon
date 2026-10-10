# qhy-camera on Linux: QHY178M + CFW, 2026-10-10 (CoolerOn after a connect's init)

This is a recorded Linux ConformU run against the same physical QHY178M + CFW as the
[2026-10-09 record](../2026-10-09-qhy-camera-qhy178m-cfw-linux-connect/README.md),
on the change that makes `CoolerOn` report what survived a connect's `InitQHYCCD`
([K4](../../services/qhy-camera.md#cooling), PR #1452), together with that PR's
review fixes.

The 2026-10-09 record showed the defect: with `disable_auto_cooler=true` in the SDK's
`qhyccd.ini`, a plain reconnect left the TEC off while `CoolerOn` still read true.
Next to the ConformU run are the probes that measure what the init does to a running
TEC under both settings, and what `CoolerOn` now reports after it.

## What was tested

| | |
|---|---|
| Commit | [`e8663a15`](https://github.com/rusty-photon/rusty-photon/commit/e8663a15) (branch `fix/qhy-camera-connect-owns-device`, PR #1452), on `main` at `7fdbf4e6` |
| Service | `qhy-camera`, **real-SDK** build (default features), `cargo run -p qhy-camera -- --log-level debug` (dev profile); rustc 1.99.0 (b940084d7 2026-09-28) |
| SDK | QHYCCD SDK **26.06.04**: `/usr/local/lib/libqhyccd.so` → `libqhyccd.so.26.6.4.16`, sha256 `f51b92f9189fae7707e98ad334cf52d3c1493a6485f33394b39a18a3f4d5c738` (byte-identical to every earlier QHY record) |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.8-200.fc44 |
| Camera | QHY178M-Cool (USB `1618:c179`, SuperSpeed), SDK id `QHY178M-222b16468c5966524`, with its 7-slot CFW. It sits on USB3 port 3 of the Pegasus PPBA Gen2's embedded hub, and its 12 V comes from the PPBA's Quad 12V output, driven through `ppba-driver` (`cargo run -p ppba-driver`) |
| ConformU | **4.5.0** build 53834.49ab847 (the latest release), against `http://127.0.0.1:11121/api/v1/{camera,filterwheel}/0` |

The Quad 12V output was switched on 30 s before the service started, and both devices
enumerated on the first start (`cameras=1 filter_wheels=1`). Three 10 ms light frames
were taken before ConformU started, each ready in 2.6 s. The ConformU run used the
SDK's defaults: no `qhyccd.ini` in the service's working directory.

## Verdicts

| Device | Suite | Result |
|---|---|---|
| Camera | `alpacaprotocol` | 0 errors, 0 issues, 16 information messages ([log](alpacaprotocol-camera.log)) |
| Camera | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-camera.log), [results](conformance-camera-results.json)) |
| FilterWheel | `alpacaprotocol` | no errors, issues or information alerts ([log](alpacaprotocol-filterwheel.log)) |
| FilterWheel | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-filterwheel.log), [results](conformance-filterwheel-results.json)) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are all **0** in both results files. The 16 informational items are the set every QHY
record carries.

## The probes

The probes are Alpaca requests made with a small client; times are seconds from the
start of each probe. Each asserted its preconditions first: the Quad 12V output on
(read back from the PPBA's switch), `1618:c179` on USB, and both devices in the
service's configured-device list. `CoolerPower` is the TEC's drive, the SDK's `CurPWM`
as a percentage.

`disable_auto_cooler=true` was set the way the 2026-10-09 record set it: a copy of the
SDK's sample `qhyccd.ini` with only that key flipped, in the service's working
directory, where the SDK reads it (`Load ini filePath = <that directory>`). It was
removed afterwards.

### 1. What the init does to a running TEC, at the millisecond

Measured earlier the same day on the PR's previous commit, `0d0925e0`; the drive is
the SDK's, so the commit does not enter into it. The cooler was engaged at 0 °C, then
the camera was disconnected and reconnected, and `CoolerPower` read every 50 ms from
the instant the reconnect returned.

| `qhyccd.ini` | Drive before the reconnect | Drive 1 ms after it | Drive 3 s after it |
|---|---|---|---|
| none (SDK defaults), 2 runs | 47.5 % | 47.5 % | 47.5 % |
| `disable_auto_cooler=true`, 2 runs | 0 % | 0 % | 0 % |

The defaults leave the drive exactly where it was. With `disable_auto_cooler=true`
the drive reads zero from the first millisecond; a probe that polls every second
(sections 2 and 3, and the 2026-10-09 record) shows it at 96–100 % before the
reconnect and 0 % after. In this probe it already read 0 % *before* the reconnect:
it sent `CoolerOn` and then polled nothing for 8 s, and with that ini setting the TEC
engaged only once something read the sensor. That observation is the SDK's, recorded
as seen; this driver does not depend on it.

### 2. SDK defaults: a cooler still regulating stays reported on

The TEC was already regulating at 0 °C when this probe started (an earlier attempt
at it had been cut short, with the cooler left on).

```
CoolerOn = true (target 0 °C)   drive 50.6 %, sensor 9.9 → 4.2 °C over 15 s
PUT connected false, then true  15.81 → 16.11 s
CoolerOn right after it         true;  SetCCDTemperature 0.0
over the next 15 s              drive 50.6 → 37.3 %, sensor 3.5 → 1.3 °C, CoolerOn true throughout
```

No cooler warning was logged.

### 3. `disable_auto_cooler=true`: a cooler the init switched off reads off

```
CoolerOn = true (target 0 °C)   drive 0 → 96 % → 28 %, sensor 3.1 → −2.3 °C over 15 s
PUT connected false, then true  16.31 → 16.62 s
CoolerOn right after it         false; SetCCDTemperature 0.0
over the next 15 s              drive 0 % throughout, sensor −1.7 → 8.9 °C, CoolerOn false throughout
PUT CoolerOn = true             drive 100 % within 2 s, sensor 8.9 → 1.8 °C over 10 s, CoolerOn true
```

The service logged, once, its only `WARN` of the run:

```
18:08:01.824192Z  WARN qhy_camera::camera: the cooler a client turned on reads no drive after this
                  connect's InitQHYCCD (qhyccd.ini's disable_auto_cooler switches it off there);
                  CoolerOn reads false until a client turns it on again camera=QHY178M-222b16468c5966524
```

On `9214df9b` (the [2026-10-09 record](../2026-10-09-qhy-camera-qhy178m-cfw-linux-connect/README.md),
section 3) the same reconnect read `CoolerOn` true for the whole of that warming. Here
`CoolerOn` reads false for as long as the TEC is off, and turning it on again engages
it at the target `SetCCDTemperature` kept.

## After the run

The cooler was switched off, both services stopped, the test `qhyccd.ini` removed,
and the Quad 12V output turned off again, the box's between-sessions state.

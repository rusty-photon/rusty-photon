# qhy-camera on Linux: QHY178M + CFW, 2026-10-13 (one SDK per process, and the list)

This is a recorded Linux ConformU run against the same physical QHY178M + CFW as the
[2026-10-10 records](../2026-10-10-qhy-camera-qhy178m-cfw-linux-move-deadline/README.md).
It was taken on device claims C4 as merged: `qhy-camera`'s `usb_devices` list with
its declared filter wheel, and `qhyccd-rs` making one `Sdk` at a time in a process
([U6, U10](../../services/qhy-camera.md#device-claims-usb_devices)). ConformU ran on
the list path, the one C4 added: the camera and its wheel were served from a pasted
`doctor --devices` block, not from the no-list default.

Next to the ConformU run is the morning's exercise of the one-`Sdk` gate on the same
commit. It covers the no-list and the list starts, four reloads, `doctor --devices`
and the SIGINT stops. Each reload builds a new `Sdk`, which can only open once the
old server's has been dropped and has released the process's SDK resource.

## What was tested

| | |
|---|---|
| Commit | [`570dd5a0`](https://github.com/rusty-photon/rusty-photon/commit/570dd5a0) (`main`, device claims C4, PR #1479), built from a detached worktree for each session |
| Service | `qhy-camera`, **real-SDK** build (default features), `cargo run -p qhy-camera -- --config <scratch copy>` (dev profile) with `RUST_LOG=qhy_camera=debug,qhyccd_rs=debug,info`; rustc 1.99.0 (b940084d7 2026-09-28) |
| SDK | QHYCCD SDK **26.06.04**: `/usr/local/lib/libqhyccd.so` → `libqhyccd.so.26.6.4.16`, sha256 `f51b92f9189fae7707e98ad334cf52d3c1493a6485f33394b39a18a3f4d5c738` (byte-identical to every earlier QHY record) |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.8-200.fc44 |
| Camera | QHY178M-Cool (USB `1618:c179`, SuperSpeed), SDK id `QHY178M-222b16468c5966524`, with its 7-slot CFW (`CFW-QHY178M-222b16468c5966524`). It sits on USB3 port 3 of the Pegasus PPBA Gen2's embedded hub (`pci-0000:c1:00.4-usbv3-0:1.3`), and its 12 V comes from the PPBA's Quad 12V output, driven through `ppba-driver` (`cargo run -p ppba-driver`) |
| Config (ConformU) | `usb_devices: [{ device_number: 0, usb_port: "pci-0000:c1:00.4-usbv3-0:1.3", name: "C4 hw test", filter_wheel_number: 0 }]`, the block `doctor --devices` printed, pasted with the wheel declared and the `devices` map deleted |
| ConformU | **4.5.0** build 53834.49ab847 (the latest release), against `http://127.0.0.1:11121/api/v1/{camera,filterwheel}/0` |

Each session switched the Quad 12V output on and started the service about 50 s later,
past the CFW's power-up homing, which takes about 20 s (C5). Before any result counted, `lsusb`
showed `1618:c179` at SuperSpeed and the service named the real camera, not
`SIM-QHY178M`. The ConformU session took one Alpaca cycle first (below) before the
suites ran.

## Verdicts

| Device | Suite | Result |
|---|---|---|
| Camera | `alpacaprotocol` | 0 errors, 0 issues, 16 information messages ([log](alpacaprotocol-camera.log)) |
| Camera | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-camera.log), [results](conformance-camera-results.json)) |
| FilterWheel | `alpacaprotocol` | no errors, issues or information alerts ([log](alpacaprotocol-filterwheel.log)) |
| FilterWheel | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-filterwheel.log), [results](conformance-filterwheel-results.json)) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are all **0** in both results files. The 16 informational items are the set every QHY
record carries. Five camera connects and five filter wheel connects ran during the
suites, each on the list-registered device, and the service logged no `WARN` or
`ERROR` of its own. Every such line in its
log comes from `ascom_alpaca` answering ConformU's deliberately malformed and
out-of-range requests.

## The one-`Sdk` gate

Every Alpaca cycle below did the same thing. It connected camera 0 and filter wheel 0,
took a 1 s light exposure, downloaded `ImageArray` as ImageBytes, moved the wheel to
slot 2 and back to 0 (polling until `Position` was not −1), and disconnected. Every
cycle gave the same numbers to within a few milliseconds:

- camera connect 0.30 s, wheel connect 1.01 s;
- `ImageReady` 3.13 s after `StartExposure`, then 12,517,420 bytes in 0.04–0.05 s;
- the wheel at rest on slot 2 after 2.89–2.91 s, and back on 0 after 2.89 s.

Every cycle passed.

| # | Step | Outcome |
|---|---|---|
| 1 | Start with no list | `cameras=1 filter_wheels=1 placeholders=0`, ready 1.64 s after start, no re-scan warning |
| 2 | Reload by SIGHUP | rebuilt 0.654 s after the signal |
| 3 | Reload by SIGHUP, **with the camera and wheel connected** | rebuilt 1.559 s after the signal; about 0.9 s of it was closing the open camera |
| 4 | Reload by `config.apply` (a `devices` name for the camera) | `status: applying`, `reload: [devices.QHY178M-222b16468c5966524]`; rebuilt 0.703 s after the apply, and the camera's `Name` then read the new name |
| 5 | Stop by SIGINT | the process exited 0.052 s after the signal, status 0 |
| 6 | `doctor --devices` | one row, `pci-0000:c1:00.4-usbv3-0:1.3  QHY178M  QHY178M-222b16468c5966524  —  0`, and the block the ConformU config above was pasted from |
| 7 | Start with the list | `a listed port holds a QHY camera`, `a declared filter wheel is served on its camera's connection`; `cameras=1 filter_wheels=1 placeholders=0`; no `OpenQHYCCD` or `IsQHYCCDCFWPlugged` before the first connect |
| 8 | Reload by SIGHUP, on the list | rebuilt 0.050 s after the signal. The list opens nothing to build, where the no-list default probes each camera for a CFW |
| 9 | Stop by SIGINT | exited 0.052 s after the signal, status 0 |
| 10 | `qhyccd-rs` `examples/enumerate`, service stopped | exit 0 after 1.12 s, `{"index":0,"id":"QHY178M-222b16468c5966524","model":"QHY178M"}` |

The ConformU session, on the list, then started (step 7 again) and passed one cycle
before the suites ran. After them it stopped by SIGINT and exited 0.052 s after the
signal.

**What this shows of the gate.** A build tries `Sdk::try_new` first. Had an old
server's `Sdk` still been alive, that try would have answered `SdkInUse`, and the build
would have logged `waiting for a USB re-scan's QHY SDK check to finish before opening
the SDK` before waiting in `Sdk::new` for a claim nobody was left to release. No
session logged that line, or any `WARN` or `ERROR` of the service's own. Every reload
rebuilt within 1.6 s, including the reload with an open camera, the case where the old
`Sdk`'s drop has the most to close.

**Not exercised:** the wait itself. A build waits only while a failed scan's re-scan
is still inside its SDK check, which a healthy bus does not produce. The unit tests
cover it (`qhyccd-rs` `sdk_gate`).

Log excerpt, step 3:

```
15:10:53.265329Z DEBUG qhy_camera::camera: camera connected camera=QHY178M-222b16468c5966524
15:10:54.280206Z DEBUG qhy_camera::filterwheel: filter wheel connected filter_wheel=CFW-QHY178M-222b16468c5966524 slots=7
15:10:54.341488Z DEBUG qhy_camera: qhy-camera shut down
15:10:55.248633Z DEBUG qhy_camera: Loaded effective configuration: server.port=11121
15:10:55.852300Z DEBUG qhy_camera: registered Camera device camera=QHY178M-222b16468c5966524
15:10:55.852332Z DEBUG qhy_camera: registered FilterWheel device filter_wheel=QHY178M-222b16468c5966524
15:10:55.852467Z  INFO qhy_camera: Service started successfully on 0.0.0.0:11121 cameras=1 filter_wheels=1 placeholders=0
```

## After the run

Both services were stopped and the Quad 12V output turned off again, the box's
between-sessions state. The camera was still on the bus at the same device number,
so nothing had wedged. The operator's own `qhy-camera.json` was untouched: every
session ran on a scratch copy, which `config.apply` wrote to (`persisted_to`).

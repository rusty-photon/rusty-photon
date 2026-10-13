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
| SDK | QHYCCD SDK **26.06.04**: `/usr/local/lib/libqhyccd.so` → `libqhyccd.so.26.6.4.16`, sha256 `f51b92f9189fae7707e98ad334cf52d3c1493a6485f33394b39a18a3f4d5c738` (byte-identical to every earlier Linux QHY record) |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.8-200.fc44 |
| Camera | QHY178M-Cool (USB `1618:c179`, SuperSpeed), SDK id `QHY178M-222b16468c5966524`, with its 7-slot CFW (`CFW-QHY178M-222b16468c5966524`). It sits on USB3 port 3 of the Pegasus PPBA Gen2's embedded hub (`pci-0000:c1:00.4-usbv3-0:1.3`), and its 12 V comes from the PPBA's Quad 12V output, driven through `ppba-driver` (`cargo run -p ppba-driver`) |
| Config (ConformU) | `usb_devices: [{ device_number: 0, usb_port: "pci-0000:c1:00.4-usbv3-0:1.3", name: "C4 hw test", filter_wheel_number: 0 }]`, the block `doctor --devices` printed, pasted with the wheel declared and the `devices` map deleted |
| ConformU | **4.5.0** build 53834.49ab847 (the latest release), against `http://127.0.0.1:11121/api/v1/{camera,filterwheel}/0` |

Each session switched the Quad 12V output on and started the service about 50 s later,
past the CFW's power-up homing, which takes about 20 s (qhy-camera's connect contract
C5). The service logs in UTC and ConformU in local time (PDT, UTC−7): the gate
exercise ran from 08:09 to 08:14 local (15:09–15:14 UTC), the ConformU session from
08:33 to 08:36. Before any result counted, `lsusb`
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
record carries. Every connect in the suites was to the list-registered devices, and
the service logged no `WARN` or `ERROR` of its own. Every such line in its
log comes from `ascom_alpaca` answering ConformU's deliberately malformed and
out-of-range requests.

## The one-`Sdk` gate

After each start and each reload below, one Alpaca cycle ran. It connected camera 0
and filter wheel 0, took a 1 s light exposure, downloaded `ImageArray` as ImageBytes,
moved the wheel to slot 2 and back to 0 (polling until `Position` was not −1), and
disconnected. Every cycle gave the same numbers to within a few milliseconds:

- camera connect 0.30 s, wheel connect 1.01 s;
- `ImageReady` 3.13 s after `StartExposure`, then 12,517,420 bytes in 0.04–0.05 s;
- the wheel at rest on slot 2 after 2.89–2.91 s, and back on 0 after 2.89 s.

Every cycle passed.

| # | Step | Outcome |
|---|---|---|
| 1 | Start with no list | `cameras=1 filter_wheels=1 placeholders=0`, ready 1.64 s after start |
| 2 | Reload by SIGHUP | rebuilt 0.654 s after the signal |
| 3 | Reload by SIGHUP, **with the camera and wheel connected**: after step 2's cycle, a bare connect of both (no exposure, no disconnect), then the signal | rebuilt 1.559 s after the signal; 0.907 s of it lay between `qhy-camera shut down` and the next config load, the old `Sdk`'s drop closing the open camera |
| 4 | Reload by `config.apply` (a `devices` name for the camera) | `status: applying`, `reload: [devices.QHY178M-222b16468c5966524]`; rebuilt 0.703 s after the apply, and the camera's `Name` then read the new name |
| 5 | Stop by SIGINT | the process exited 0.052 s after the signal, status 0 |
| 6 | `doctor --devices` | one row, `pci-0000:c1:00.4-usbv3-0:1.3  QHY178M  QHY178M-222b16468c5966524  —  0`, and the block the ConformU config above was pasted from |
| 7 | Start with the list | `a listed port holds a QHY camera`, `a declared filter wheel is served on its camera's connection`; `cameras=1 filter_wheels=1 placeholders=0` |
| 8 | Reload by SIGHUP, on the list | rebuilt 0.050 s after the signal, where the no-list reloads with nothing open took 0.65–0.70 s. The no-list build opens each camera to probe it for a CFW and the list's is designed to open none (U1), which would account for the gap, but the timing does not show it (see *Not shown either*, below) |
| 9 | Stop by SIGINT | exited 0.052 s after the signal, status 0 |
| 10 | `qhyccd-rs` `examples/enumerate`, service stopped | exit 0 after 1.12 s, `{"index":0,"id":"QHY178M-222b16468c5966524","model":"QHY178M"}` |

The ConformU session, on the list, then started (step 7 again) and passed one cycle
before the suites ran. After them it stopped by SIGINT and exited 0.052 s after the
signal.

**What this shows of the gate.** A server drops its `Sdk` when it stops serving, before
the next build begins (`BoundServer::start`). The drop closes any open camera and runs
`ReleaseQHYCCDResource`, and the claim the next build's `Sdk::try_new` needs is a field
of the dropped `Sdk`, freed when the drop returns. So a reload cannot meet the old
server's claim. The only claim a build can meet is a failed scan's re-scan try's (*Not
exercised*, below), and none was alive here. The absence of the build's `SdkInUse`
warning therefore shows nothing on its own.
What the reloads do show is three things about the real SDK's teardown:

- **The drop returns.** From `qhy-camera shut down` to the next build's config load
  took 0.1 ms or less on the reloads with nothing open, and 0.907 s on step 3's, with
  the camera and wheel connected.
- **It reports no failure.** The gate exercise ran with `qhyccd_rs=debug`, and its logs
  hold no `WARN` or `ERROR` line from any target. That includes the `error!` lines a
  failed camera close or `ReleaseQHYCCDResource` would log during the drop.
- **The SDK comes back after it.** Each next build initialised the SDK again, found the
  camera, and served it through a full cycle.

**Not exercised:** the build's wait itself. A build meets a live `Sdk` only when a
failed scan's re-scan is still inside its SDK check, which a healthy bus does not
produce. `qhy-camera`'s
`take_sdk_tests::a_build_waits_for_a_live_sdk_to_drop_before_opening_its_own` covers
it, over `qhyccd-rs`'s `sdk_gate` tests.

**Not shown either:** that the list start opens no camera. The SDK's own trace prints
only its initialisation, and the service logs no open below a connect, so neither log
tells an open from none. Even the no-list start's CFW probe, which does open the
camera, leaves no line. That the list path opens nothing rests on U1 and on the
BDD scenario *A listed camera is not opened before a client connects it*. There the
simulated camera has left the bus before the start, so any open would fail, and the
list registers it all the same.

Log excerpt, step 3 (UTC):

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
between-sessions state. The camera was still on the bus at the same USB device number
(`Bus 002 Device 025` in `lsusb`), so it had not re-enumerated, and nothing had
wedged. The operator's own `qhy-camera.json` was untouched: every
session ran on a scratch copy, which `config.apply` wrote to (`persisted_to`).

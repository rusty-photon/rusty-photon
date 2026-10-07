# zwo-camera on Windows: a camera that leaves the bus, 2026-10-07

This is the Windows companion to the
[2026-10-06 Linux record](../2026-10-06-zwo-camera-departure-linux/README.md), made
with the same three physical cameras on the same PPBA. It covers issue #1411
(PR #1417): a ZWO camera that loses its power or its cable while connected must read
disconnected ([C6](../../services/zwo-camera.md#enumeration--connection-lifecycle)).
It has three parts:

- **The five departure probes of the Linux record, repeated on Windows.**
- **A sweep of power cuts across one exposure**, looking for the blank frame the
  Linux record could not provoke.
- **A ConformU run of the same build on all three cameras.**

The cameras really lost power. The ASI178MM and ASI120MC-S hang off a USB2 hub on
the Pegasus PPBA's switchable USB2 port, and the PPBA's `USB Hub` switch (`PU`)
cut that port's power from the host. The ASI1600MM-Cool sits on the PPBA hub's USB3
side, which `PU` does not switch, and served as the control throughout.

## What was tested

| | |
|---|---|
| Commit | [`7bf0d821`](https://github.com/rusty-photon/rusty-photon/commit/7bf0d821) (`main`), which contains PR #1417 (merge [`e0bc5cef`](https://github.com/rusty-photon/rusty-photon/commit/e0bc5cef)) and camera-core #1425. `services/zwo-camera`, `crates/zwo-rs`, `crates/libzwo-sys`, `crates/rusty-photon-camera-core` and `Cargo.lock` are identical at both commits |
| Service | `zwo-camera`, **real-SDK** build (default features), `cargo run -p zwo-camera -- --config <file> --log-level debug` (dev profile), with `ZWO_SDK_LIB_DIR` and `LIBCLANG_PATH` set; rustc 1.99.0 (b940084d7 2026-09-28). The config bound the service to `127.0.0.1:11122` only |
| SDK | ASI SDK **1.41** for Windows: `ASI_Windows_SDK_V1.41` `lib\x64\ASICamera2.dll`, file version `1, 41, 0, 0`, sha256 `0c8778c3cce2012961b079e3c7d0d8348a8b3823939335d9e98148cb5d5dc34a` (the copy staged for the [2026-07-27 Windows record](../2026-07-27-zwo-camera-asi1600mm-cool-windows/README.md)), placed next to the binary |
| Driver | ZWO ASI camera driver **3.28.0.0** (`asicamusb3.inf`), installed in July. All three cameras bound as `Class Image`, status OK |
| Platform | Windows 11 Pro 25H2 x64 (build 26200), a KVM guest (QEMU 10.2.2, libvirt 12.0.0, machine `pc-q35-10.1`) on the Fedora 44 dev box. The cameras were passed through as USB `hostdev`s on the guest's `qemu-xhci` controller |
| Cameras | device 0 `ZWO:ZWO-ASI1600MM-Cool:noserial-0` (USB3, the control); device 1 `ZWO:ZWO-ASI178MM:1915d5081b090900`; device 2 `ZWO:ZWO-ASI120MC-S:1f19470620070900` (both on the switched USB2 port). Devices 1 and 2 are the other way round from the Linux record; see *Device numbers* below |
| Departure | PPBA Gen2C switch 4 (`USB Hub`), driven from the host through `cargo run -p ppba-driver` on loopback, then an immediate `virsh detach-device` of each camera's hostdev (see *How a departure reached the guest*). The PPBA's own serial link survived every toggle and was checked after each one. Quad 12V stayed off throughout |
| ConformU | **4.5.0** build 53834.49ab847 (the current upstream release), installed in the guest and run there against `http://127.0.0.1:11122/api/v1/camera/{0,1,2}` |

The guest's installed `rusty-photon-zwo-camera` service, and the sentinel and rp
services that would supervise or reconnect it, were stopped for the run.

## Verdicts

| Camera | `alpacaprotocol` | `conformance` |
|---|---|---|
| ASI1600MM-Cool (device 0) | no errors, issues or information alerts ([log](asi1600mm-cool-alpacaprotocol.log)) | *"no errors, warnings or issues found"*; every member within its target response time ([log](asi1600mm-cool-conformance.log), [results](asi1600mm-cool-conformance-results.json)) |
| ASI178MM (device 1) | no errors, issues or information alerts ([log](asi178mm-alpacaprotocol.log)) | *"no errors, warnings or issues found"*; every member within its target response time ([log](asi178mm-conformance.log), [results](asi178mm-conformance-results.json)) |
| ASI120MC-S (device 2) | no errors, issues or information alerts ([log](asi120mc-s-alpacaprotocol.log)) | *"no errors, warnings or issues found"*; every member within its target response time ([log](asi120mc-s-conformance.log), [results](asi120mc-s-conformance-results.json)) |

`ErrorCount`, `IssueCount`, `ConfigurationAlertCount` and `TimingIssuesCount` are all
**0** in all three results files. The suites ran after the departure probes and before
the sweep, from the same service instance, so every connect in them went through the
open by identity. The service logged ten departures over the whole run, each at a
planned power cut, and none while ConformU ran. Apart from those (a `WARN` pair each)
and the startup note that the ASI1600MM-Cool has no serial, its only `WARN` lines are
the Alpaca library's, for the protocol suite's deliberate unknown parameters.

## How a departure reached the guest

The first attempt at phase 1 failed its precondition, and the probe stopped. With the
port cut, both cameras were gone from the host's `lsusb` and the kernel logged `USB
disconnect`, but 31 s later the guest still listed both as present and OK, and QEMU's
`info usb` still showed them on the guest's xHCI. QEMU notices that a passed-through
device has gone from the host only when an I/O on it fails, and an idle camera has
no I/O in flight. A Windows machine with the camera on its own port would see the
removal at once.

So each departure here is two steps, done together: the PPBA cuts the port, and as
soon as the cameras have gone from the host's bus, `virsh detach-device` takes them
off the guest's bus. Windows sees the surprise removal straight away, and the camera
really is unpowered. That second fact makes this closer to a cable pull than the QHY
[Windows record](../2026-10-06-qhy-camera-qhy178m-cfw-windows-departure/README.md),
whose camera stayed powered through a hypervisor detach. In particular the way back
here is a real power cycle: switch the port on, then `virsh attach-device` again,
because a hostdev stays pinned to the host device number it was attached with. No
camera was ever re-attached without a power cycle, the case that wedged the QHY.

The aborted attempt was cleaned up (a detach, both sessions released, the port
switched back on, a re-attach), and the service was restarted so that its log starts
with the run below.

## The probes

Five phases, run in order against one service instance, after a baseline in which
every camera connected and took a frame
([transcript](departure-probes.txt)). Times are local (UTC−7). The service log is UTC
from the guest's clock, which ran about 1.0 s behind the host's. Each phase asserted
its precondition on both sides: the host's `lsusb` and the guest's PnP list
(`Get-PnpDevice -PresentOnly`, VID `03C3`, status OK).

### 1. Idle: only a failure asks

```
--- reads on a hidden departure: answered, and nothing asks ---
08:25:25  GET 1/ccdtemperature               value=0.0          err=0x0
08:25:25  GET 1/gain                         value=0            err=0x0
08:25:25  GET 1/connected                    value=True         err=0x0
08:25:26  GET 2/ccdtemperature               value=26.7         err=0x0
--- the first failure: an exposure's arm ---
08:25:27  PUT 1/startexposure                value=None         err=0x0
08:25:29  GET 1/connected                    value=False        err=0x0
08:25:29  GET 1/camerastate                  value=None         err=0x407 NOT_CONNECTED
08:25:29  GET 1/gain                         value=None         err=0x407 NOT_CONNECTED
08:25:30  PUT 2/startexposure                value=None         err=0x0
08:25:32  GET 2/connected                    value=False        err=0x0
```

```
15:25:26.349492Z WARN zwo_camera::backend: the camera has left the bus; it reads disconnected until a
                 client releases it camera=ZWO:ZWO-ASI178MM:1915d5081b090900
                 failure=failed to set offset: ASI camera SDK error: general error (e.g. value out of valid range)
15:25:28.902588Z WARN zwo_camera::backend: the camera has left the bus; ...
                 camera=ZWO:ZWO-ASI120MC-S:1f19470620070900 failure=ASI camera SDK error: invalid camera ID
```

This is the Linux sequence. Reads kept answering from the SDK's memory, with Windows
already reporting the cameras removed. The ASI178MM's arm failed `GENERAL_ERROR`
(writing the offset here; the gain on Linux), and the check's rescan found it gone.
That rescan also dropped the ASI120MC-S from the SDK's list, so its arm failed
`INVALID_ID`, and its own check confirmed. `Connected = false` then released each
session, and `Connected = true` failed with `NOT_CONNECTED` while the cameras were
still gone. The control camera took a frame.

### 2. Back: a plain reconnect finds each camera by its identity

```
08:26:10  PUT 1/connected (true)             err=0x0  (380.1 ms)
08:26:11  GET 1/gainmax                      value=510    (the ASI178MM's own)
08:26:13  GET 1/imageready                   value=True
08:26:14  PUT 2/connected (true)             err=0x0  (751.3 ms)
08:26:15  GET 2/gainmax                      value=100    (the ASI120MC-S's own)
08:26:16  GET 2/imageready                   value=True
```

The cameras came back with new host device numbers and were attached to the guest
afresh. Each open rescanned and matched its camera by name and serial, and `GainMax` shows each
device on its own body.

### 3. A departure 3 s into a 10 s exposure

```
08:26:42  PUT 1/startexposure (10 s)         err=0x0
08:26:45  PPBA setswitch USB Hub=False
08:26:51  t+  8.9s 1/connected               value=True
08:26:53  t+ 10.2s 1/connected               value=False
```

```
15:26:41.216523Z DEBUG zwo_camera::backend: exposure armed its gain and offset gain=Some(0) offset=Some(10)
15:26:51.792981Z  WARN zwo_camera::backend: the camera has left the bus; ... camera=ZWO:ZWO-ASI178MM:1915d5081b090900
                 failure=exposure failed
```

The port was cut 3.2 s into the frame. The watcher read only `Connected`, which never
reaches the SDK. The session was marked lost 10.6 s after the arm, when the frame was
due: the capture's readout poll found the exposure failed (`ASI_EXP_FAILED`), asked,
and found the camera gone, with no client call. The idle ASI120MC-S still read
connected, as C6 says: nothing on it had failed yet.

### 4. Back, with a session held across the departure

The ASI120MC-S's session had been open since phase 2. After the port came back and the
cameras were re-attached, its next exposure failed `INVALID_ID`, and the check found
the old ID no longer naming a listed camera. It read disconnected, and a reconnect
brought both devices back on their own bodies (`GainMax` 510 and 100).

### 5. Off and on again, with nothing failing in between

Both sessions were fresh and had each taken a frame. The USB2 port was then switched
off for 3 s and back on, with the cameras detached and re-attached, and with no SDK
call in between, so no rescan ran while they were gone.

```
08:27:58  GET 1/connected                    value=True
08:27:59  PUT 1/startexposure                err=0x0
08:27:59  GET 1/camerastate                  value=None  err=0x407 NOT_CONNECTED
08:28:00  GET 1/connected                    value=False
08:28:01  PUT 2/startexposure                err=0x0
08:28:01  GET 2/camerastate                  value=None  err=0x407 NOT_CONNECTED
08:28:02  GET 2/connected                    value=False
```

```
15:27:58.420617Z WARN ... camera=ZWO:ZWO-ASI178MM:1915d5081b090900
                 failure=failed to set offset: ASI camera SDK error: general error (e.g. value out of valid range)
15:27:59.934783Z WARN ... camera=ZWO:ZWO-ASI120MC-S:1f19470620070900 failure=ASI camera SDK error: camera not open
```

The ASI178MM's arm failed `GENERAL_ERROR`. The check's rescan listed the returned
camera afresh, so its old ID answered `CAMERA_CLOSED`, and the session ended. The
ASI120MC-S's arm then met `CAMERA_CLOSED` (*camera not open*) directly. Both
reconnected onto their own bodies and took frames: one failed exposure per camera,
as on Linux.

The control camera, on USB3, kept its session and took a frame after every phase.

## The blank frame: a sweep of cuts across one exposure

C6 discards an all-zero frame from a departed camera, because a QHY readout returned
one on Windows. The Linux record never saw an ASI readout do so, so this run looked
for one. The ASI178MM was set to its full frame (3072 × 2064 RAW16, 12.7 MB through
USB2) and given 0.5 s exposures, and the port was cut at stepped delays after each
exposure started ([transcript](departure-timing-sweep.txt)). The times below come from
the service log (the arm) and ppba-driver's log (the cut), with the guest's clock
corrected by +1.0 s. That correction is good to about ±0.25 s.

| Cut after the arm | Where it fell | Outcome |
|---|---|---|
| 0.25 s | integrating | `exposure failed` at the readout poll, lost 1.4 s after the cut |
| 0.52 s | the end of the integration | `exposure failed`, lost 1.1 s after the cut |
| 0.71 s | readout / transfer | `exposure failed`, lost 1.1 s after the cut |
| 0.93 s | readout / transfer | `exposure failed`, lost 0.7 s after the cut |
| 1.11 s | after the frame completed | frame published (`ImageReady = true`); the idle camera then read connected, as C6 says |
| 1.46 s | after the frame completed | the same |

Even with the clock's uncertainty, the 0.93 s cut fell after the 0.5 s integration
ended and before the frame completed. The 0.71 s cut almost certainly did as well.
Every cut before completion surfaced as `ASI_EXP_FAILED` from the exposure status. In
none was a frame downloaded from a camera that had gone, so the blank-frame check was
never reached: the log has no blank frame and no `CAMERA_REMOVED` anywhere. This fits
the driver's readout, which downloads with `ASIGetDataAfterExp` only once the exposure
status reads success. A cut anywhere before that turned the status to failed instead.

## Device numbers

Device numbers come from the SDK's enumeration at startup (C0). The first service
start, before any departure, numbered the ASI120MC-S 1 and the ASI178MM 2, as on
Linux. After the aborted attempt's detach and re-attach, the restart numbered them the
other way round. Both times the order matched the order in which their hostdevs had
been attached to the guest. The UniqueIDs follow the bodies, and C6's open by identity
found each camera by its serial throughout.

## Still owed

- **Nothing on the camera side of #1411 that this method can reach.** Windows
  answers a departure the way Linux does, on the same SDK version. The one difference
  from a bare-metal machine is how Windows learned of each departure: through the
  hypervisor detach made at the cut, rather than from its own hub driver.
- **The blank frame stays unobserved on ASI hardware**, on both platforms. The rule
  stays as a guard: it costs nothing on a real frame, and the evidence that a readout
  can return one comes from another vendor's SDK.
- zwo-focuser's departure is #1431.

## After the run

All three cameras were disconnected, the service was stopped, the cameras were
detached from the guest, and the guest was shut down. Its services start
automatically at its next boot. ppba-driver was stopped. The PPBA was left as found:
the `USB Hub` switch on and the Quad 12V output off.

## Files

- [`departure-probes.txt`](departure-probes.txt): the baseline and the five phases
  above, as the client printed them.
- [`departure-timing-sweep.txt`](departure-timing-sweep.txt): the six cuts of the
  sweep.
- `asi1600mm-cool-*`, `asi178mm-*`, `asi120mc-s-*`: ConformU's `alpacaprotocol` and
  `conformance` logs, and the `conformance` results, per camera.

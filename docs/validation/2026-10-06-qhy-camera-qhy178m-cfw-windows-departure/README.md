# qhy-camera on Windows: QHY178M + CFW, 2026-10-06 (a camera that leaves the bus)

This is the Windows companion to the
[2026-10-05 Linux record](../2026-10-05-qhy-camera-qhy178m-cfw-linux-departure/README.md),
made against the same physical QHY178M + CFW. It was taken on the change that makes a
camera that has left the USB bus read disconnected
([C9 / FW4](../../services/qhy-camera.md#enumeration--connection-lifecycle), issue
#1329, PR #1405).

Next to the ConformU run are the hardware probes:

- a 12 V cut that leaves the camera on USB (the negative control);
- the camera taken off the guest's USB bus while idle, twice, with the reconnect after it;
- the camera taken off in the middle of a 30 s exposure;
- a readout that stalls inside `GetQHYCCDSingleFrame` and returns **success with an
  all-zero frame**. Measured twice before the fix, then once on the fix it led to
  (`a953fb2d`), where the frame is discarded.

## What was tested

| | |
|---|---|
| Commits | [`79cc1600`](https://github.com/rusty-photon/rusty-photon/commit/79cc1600) for ConformU and every probe up to and including the mid-exposure departure; [`a953fb2d`](https://github.com/rusty-photon/rusty-photon/commit/a953fb2d) (the blank-frame rule) for the last probe. Both on branch `fix/qhy-camera-camera-left-bus-1329`, PR #1405 |
| Service | `qhy-camera`, **real-SDK** build (default features), `cargo run -p qhy-camera -- --config <file> --log-level debug` (dev profile), with `QHYCCD_SDK_DIR` and `LIBCLANG_PATH` set; rustc 1.99.0 (b940084d7 2026-09-28). The config bound the service to `127.0.0.1:11121` only |
| SDK | QHYCCD SDK **26.06.04**: `sdk_win64_26.06.04\x64\qhyccd.dll`, file version `26, 6, 4, 16`, sha256 `c7cea0039c3719388dcbb38f02524d4bdc6aaa827495056a2ec3b5bb24551d5f` (the same file as the [2026-07-28 Windows record](../2026-07-28-qhy-camera-qhy178m-cfw-windows/README.md)), placed next to the binary. The preflight (WD1) tries the exe's directory before the All-in-One install and `PATH`, and its log line confirmed it bound that copy |
| Driver | QHY All-in-One 25.06.16 `qhycameras.inf`, provisioned in July. The camera enumerated as `Status OK, Class AstroCams` |
| Platform | Windows 11 Pro 25H2 x64 (build 26200), a KVM guest (QEMU 10.2.2, libvirt 12.0.0, machine `pc-q35-10.1`) on the Fedora 44 dev box. The camera was passed through as a USB `hostdev` on the guest's `qemu-xhci` controller |
| Camera | QHY178M-Cool (USB `1618:c179`, "Q178-Cool"), SDK id `QHY178M-222b16468c5966524`, with its 7-slot CFW. It sits on USB3 port 3 of the Pegasus PPBA Gen2's embedded VL813 hub. Its 12 V comes from the PPBA's Quad 12V output, driven from the host through `ppba-driver` (`cargo run -p ppba-driver`, bound to loopback) |
| ConformU | **4.5.0** build 53834.49ab847, installed in the guest for this run and run there against `http://127.0.0.1:11121/api/v1/{camera,filterwheel}/0` |

The guest's installed `rusty-photon-qhy-camera` service was stopped for the run and
started again afterwards.

## Verdicts

| Device | Suite | Result |
|---|---|---|
| Camera | `alpacaprotocol` | 0 errors, 0 issues, 16 information messages ([log](alpacaprotocol-camera.log)) |
| Camera | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-camera.log), [results](conformance-camera-results.json)) |
| FilterWheel | `alpacaprotocol` | no errors, issues or information alerts ([log](alpacaprotocol-filterwheel.log)) |
| FilterWheel | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-filterwheel.log), [results](conformance-filterwheel-results.json)) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount` are all
**0** in both results files. The 16 informational items are the set every QHY record
carries.

Every capability probe asks C9's presence question, so every `HasShutter`,
`CanSetCCDTemperature` and `SensorType` read ConformU made asked it too, on Windows.
Across both suites the service logged no departure and no `WARN` or `ERROR` of its
own. The only `WARN` lines are the Alpaca library's, for the protocol suite's
deliberate unknown parameters and for members the camera does not implement.

## How the camera left the bus

A departure here is a **hypervisor detach**: `virsh detach-device` takes the camera off
the guest's virtual xHCI. Windows sees the same surprise removal a cable pull causes,
and the timing is exact and repeatable. The difference from a pull is that the camera
stays powered on the host.

That difference matters only for the way back. A camera re-attached to the guest
**without a power cycle** comes back stale. Its first temperature read was −537.6 °C,
then 0.0 °C. The readout after it stalls, and the host's kernel logs `device descriptor
read/8, error -110` until it drops the device about 85 s later. This happened all three
times it was tried. It is an artefact of the method, not of the driver: no driver
reaches the camera's port while it is detached. The probes below record which
re-attaches followed a physical power cycle; the last probe uses a stale re-attach on
purpose.

## The probes

The probes are Alpaca requests made from inside the guest through the QEMU guest
agent, with the orchestration on the host. Each row is one exchange, with times in
local time (UTC−7); the service log is in UTC. Each phase asserted its precondition
with the guest's PnP list (`Get-PnpDevice -PresentOnly`, VID `1618`): the camera on the
guest's bus for the negative control, and off it for the departures.

### 1. Negative control: 12 V cut, camera still on USB

```
--- cutting the PPBA Quad 12V (camera logic stays on USB) ---
17:45:31  PUT setswitch                value=None           err=0x0
17:45:31  GET getswitch                value=False          err=0x0
17:45:34  GET ccdtemperature           value=23.0           err=0x0
17:45:39  GET cansetccdtemperature     value=True           err=0x0
17:45:40  PUT startexposure            value=None           err=0x0
17:45:43  GET imageready               value=True           err=0x0
17:45:44  GET connected                value=True           err=0x0   (camera)
17:45:44  GET connected                value=True           err=0x0   (wheel)
{"camera_connected": true, "wheel_connected": true, "departures_logged": 0}
NEGATIVE CONTROL PASSED: 12V cut, camera on USB, not marked lost
```

The QHY178M runs its logic off USB, so with its 12 V cut it stayed on the bus, answered
every call and took a frame. Nothing marked it lost.

### 2. Idle departure, and the question C9 relies on

```
17:47:17  virsh detach-device: Device detached successfully
--- lazy window: nothing has reached the SDK yet ---
17:47:18  GET connected                value=True           err=0x0
17:47:19  GET gain                     value=51             err=0x0   (cache-served)
--- first SDK call ---
17:47:19  GET ccdtemperature           value=None           err=0x407 'The communications channel is not connected.'
--- after it ---
17:47:19  GET connected                value=False          err=0x0
17:47:21  GET connected                value=False          err=0x0   (wheel)
{"first_call_error": 1031, "camera_connected": false, "wheel_connected": false, "departures_logged": 1}
--- release: Connected=false on both, the camera still gone ---
17:47:22  PUT connected                value=None           err=0x0   (both)
--- reconnect while the camera is still gone ---
17:47:23  PUT connected                value=None           err=0x407
```

```
00:47:18.577792Z DEBUG qhyccd_rs::camera: control=IsControlAvailable { control: Cooler }
00:47:18.577819Z DEBUG qhyccd_rs::camera: control=IsControlAvailable { control: CamSingleFrameMode }
00:47:18.577831Z  WARN qhy_camera::backend: camera no longer answers for a control its connect required; it has left the bus, ...
00:47:22.910228Z ERROR qhyccd_rs::camera: error=Sdk { op: "open_camera" }
```

**On Windows, too, the SDK stops answering for `CamSingleFrameMode` once the camera
has gone.** This was the Windows half of the question C9 left open. The cooler probe answered "absent" first, which is the
"no cooler" negative #1329 reported from rig2. The presence question that follows
turned it into a departure. The same sequence, repeated at 17:55:35 with the same
result, is in section 4.

### 3. A stalled readout reports success with a blank frame

After the detach in section 2 the camera was re-attached **without a power cycle**, and
a plain reconnect succeeded. Then its first temperature read was −537.6 °C, the 0.01 s
exposure that followed never finished reading out, and the host logged descriptor
timeouts until it dropped the device. On the driver's side:

```
00:47:49.337567Z DEBUG qhy_camera::camera: gain and offset armed gain=Some(30) offset=Some(0)
00:49:15.219356Z DEBUG qhy_camera::camera: frame read width=3056 height=2048 bits_per_pixel=16 channels=1 buffer_bytes=27116352
```

`GetQHYCCDSingleFrame` blocked for 86 s, then returned **success**, 1 s before the
host dropped the device (`usb 2-1.3: USB disconnect` at 17:49:16 local). It did not
hang, but it did not fail either. The driver published the frame as `ImageReady =
true`, and since no call had failed, C9 never asked. The departure was caught only by
the next SDK call, 21 s later.

The same thing happened again after the repeat in section 4 (readout from 17:55:52,
`frame read` at 00:57:17.514Z, host disconnect at 17:57:18). That frame's statistics,
read from the stored `ImageArray`, which makes no SDK call:

```
rank=2 3056x2048 transmission=8 pixels=6258688 min=0 max=0 mean=0.0 zeros=6258688 (100.0%)
```

**Every pixel was zero.** A genuine frame from this camera, for comparison:
`min=4 max=65528 mean=43.7 zeros=0 (0.0%)`. This led to `a953fb2d`, whose readout
counts a blank frame as a failure (section 6).

The first of these two stalls overlapped two PPBA `USB Hub` (switch 4) toggles made by
another session for the ZWO cameras on the hub's USB2 side. The repeat in section 4 had
no PPBA activity at all and stalled the same way, which rules the toggles out.

### 4. Recovery after a power cycle, and a second idle departure

After a physical power cycle (the USB cable pulled and replugged; the 12 V stayed on),
the camera came back as `c178` at full speed on the hub's USB2 side, and its firmware
load failed (`fxload` exit 224). A second, firmer replug brought it back as a
SuperSpeed `c179`. After a re-attach to the guest and a release of the lost session, a plain reconnect
found it: 18.8 °C, a frame, and the wheel at slot 0.

The second idle departure (detach at 17:55:35) reproduced section 2 exactly: the lazy
window, then `NOT_CONNECTED` at the first SDK call, both devices disconnected, one
`WARN`, a clean release and the C2 refusal.

**One reopen failed until a restart.** After the second stall, the camera was
power-cycled and re-attached before the driver had noticed it was gone, because the
readout had returned "success" and nothing had reached the SDK since. A release then
closed the stale session cleanly, but every `OpenQHYCCD` after it failed
(`open_camera`, four times), while a fresh process (`qhy-camera doctor`) saw the
camera. A service restart re-enumerated it. This is one sample, and it is recorded as
an observation, not a rule.

### 5. Departure mid-exposure

A 30 s light frame was started at 18:03:26, and the camera was detached at 18:03:37.
The watcher read only `Connected` and `CameraState`, neither of which reaches the SDK.

```
t+  12.7s  Connected=True  CameraState=2 err=0x0
t+  31.1s  Connected=False CameraState=None err=0x407
{"warnings_logged": 1, "final_connected": false}
```

```
01:03:56.136780Z ERROR qhyccd_rs::camera: error=Sdk { op: "get_remaining_exposure_us" }
01:03:56.136853Z DEBUG qhyccd_rs::camera: control=IsControlAvailable { control: CamSingleFrameMode }
01:03:56.136865Z  WARN qhy_camera::backend: camera no longer answers for a control its connect required; it has left the bus, ...
01:03:56.137013Z  WARN qhy_camera::camera: mid-exposure SDK error error=the camera left the bus before its frame was read out
01:03:57.466935Z  WARN qhy_camera::backend: connect refused: the camera has left the bus and another device on its connection still holds it; ...
```

The capture slept through its exposure. When the frame was due, the progress poll was
the first call to fail, and it asked the presence question itself (`712adf40`). The
readout was skipped: no `get_image_size` call followed. A camera reconnect while the
wheel still held the lost session got the FW4 refusal. Releasing the wheel cleared it.

### 6. The blank-frame rule on hardware (`a953fb2d`)

The camera had not been power-cycled since section 5's detach, which is the stale state
of section 3. It was re-attached, the service was restarted at `a953fb2d` so that it
enumerated the camera (`cameras=1 filter_wheels=0`: on the stale camera the CFW did
not enumerate, and the scan took 23 s), and a 0.01 s frame was taken.

```
23:07:16  PUT connected                value=None           err=0x0
23:07:27  GET ccdtemperature           value=0.0            err=0x0 (10172.9 ms)
23:07:27  PUT startexposure            value=None           err=0x0
23:09:11  GET connected                value=False          err=0x0
23:09:11  GET imageready               value=None           err=0x407
```

```
06:08:53.535697Z DEBUG qhy_camera::camera: frame read width=3056 height=2048 bits_per_pixel=16 channels=1 buffer_bytes=27116352
06:08:53.574441Z DEBUG qhyccd_rs::camera: control=IsControlAvailable { control: CamSingleFrameMode }
06:08:53.574591Z  WARN qhy_camera::backend: camera no longer answers for a control its connect required; it has left the bus, ...
06:08:53.575540Z  WARN qhy_camera::camera: mid-exposure SDK error error=the camera left the bus during its readout; the frame was discarded
```

The readout again returned "success" after 86 s. The blank check and the presence
question took 39 ms, the worst case: a full 12.5 MB all-zero frame in a debug build. On
a real frame the scan stops at its first non-zero byte. The frame was discarded, and
nothing was published as `ImageReady`. After a final power cycle, re-attach and
restart, both devices reconnected (22.0 °C, slot 0) and took a genuine frame
(`min=4 max=65528 mean=60.5 zeros=0`).

## Still owed

- **A physical cable pull on Windows.** Every departure here was a hypervisor detach,
  which Windows sees as a surprise removal. rig2's QHY600M, which drops off USB when its
  12 V goes, is the field case.
- **The reopen that failed after a replug the driver had not noticed** (section 4). It
  needs more than one sample before it is a rule.

## After the run

Both devices were disconnected and the service stopped. The guest's installed
`rusty-photon-qhy-camera` service was started again, the camera was detached from the
guest, and the guest was shut down. The Quad 12V output was turned off, the box's
between-sessions state, and the PPBA's `USB Hub` switch was left on.

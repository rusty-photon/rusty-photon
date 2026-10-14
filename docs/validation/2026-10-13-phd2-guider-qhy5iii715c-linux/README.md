# phd2-guider camera facade — QHY5III715C behind PHD2 2.6.14, Linux (device claims C6)

`phd2-guider`'s opt-in camera facade
([phd2-guider.md § Camera facade](../../services/phd2-guider.md#camera-facade-camera))
with a real guide camera behind PHD2: the field rig's QHY5III715C. ConformU
4.5.0 ran its `alpacaprotocol` and `conformance` suites, and both were
**clean**: every count in `conformance-results.json` is 0.

This is the run the
[simulator record](../2026-10-13-phd2-guider-phd2-simulator-linux/README.md)
said was still owed. Two things depend on the camera rather than on PHD2:
whether `stop_capture` cuts an exposure short, and the frame's size and
pixels. Both were measured here. **PHD2's QHY driver does not cut an
exposure short**: an aborted frame runs to its end and PHD2 reports it as a
success. The facade still discards it. The frame is a 3864 × 2192 16-bit
luminance image that PHD2 reconstructs from the colour sensor.

The run also found a deployment gap the facade recipe does not cover. The
packaged QHY udev rule gives every QHY device to the `rusty-photon` group
only, so PHD2, running as the operator, could not open the guide camera
until the operator's account was given access
([below](#deployment-finding-phd2-could-not-open-the-camera)).

The rig was indoors, so the frames show room light and no stars. Neither
suite needs stars.

## What was tested

| | |
|---|---|
| Commit | [`3f0b9a5e`](https://github.com/rusty-photon/rusty-photon/commit/3f0b9a5e) (`main`, the C6 merge) |
| Host | the field rig (Raspberry Pi 5, Raspberry Pi OS, Debian 13, aarch64) |
| Build | `cargo run --release -p phd2-guider --bin phd2-guider` on the rig (cargo 1.99.0), no features; no native SDK is involved in the facade |
| Guide camera | QHY5III715C (Sony IMX715, colour), USB `1618:0716` after its firmware load; QHY SDK id `QHY5III715C-6cf42c41a8b2f3188`. On the port it uses in service |
| PHD2 | 2.6.14, source-built on the rig with its bundled QHY camera support (QHY SDK `V20240329_10`), headless under TigerVNC as the operator. A new equipment profile, "QHY5III715C guide (no mount)": camera `QHY Camera`, mount, aux mount, AO and rotator all `None`, pixel size 1.45 µm, focal length 360 mm. Made by cloning the "Simulator" profile's keys while PHD2 was stopped (`probe/addprofile.py`). Only the camera was connected; PHD2 never looped, guided or connected a mount |
| Vendor driver | `rusty-photon-qhy-camera` was stopped for the run, so it did not hold the camera, and started again afterwards. No `usb_devices` list was written: the run tests PHD2's side of the facade, and the list path is the drivers' own (C5) |
| Service account | The operator's, not `rusty-photon`. PHD2 and the facade both ran as the operator, with `camera.image_dir` in the operator's home. The service account's sandbox and the shared-directory recipe were tested in the simulator record and are not repeated here |
| Config | REST on `127.0.0.1:11230` and the facade on `127.0.0.1:11228`. `camera.pixel_size_x_um` / `_y_um` 1.45, the value `qhy-camera` reads from the SDK for this camera. Defaults otherwise (`capture_grace` 10 s) |
| Device identity | `UniqueID` `761bb4e5-8283-4add-ba39-63b2d6f05cd9`, minted into the config file at the first enabled start (`service.log` line 5) |
| ConformU | 4.5.0 (Build 53834), the latest release on the day. It ran on the dev box (Fedora Linux 44 x86_64) through an SSH tunnel to the rig's loopback port. The settings file was `{"SettingsCompatibilityVersion": 1}` |

## Results

| Suite | Result |
|---|---|
| `alpacaprotocol` | 0 errors, 0 issues, 20 information messages |
| `conformance` | `ErrorCount` 0, `IssueCount` 0, `ConfigurationAlertCount` 0, `TimingIssuesCount` 0. One full-frame 2 s exposure, read as a 3864 × 2192 image through the tunnel in 2.26 s; every negative and out-of-sensor `StartExposure` rejected |

## What the facade serves for this camera

Read over the facade (`probe/probe-facts-direct.jsonl`, the `facts` scenario):

| Property | Value |
|---|---|
| `CameraXSize` / `CameraYSize` | 3864 / 2192, PHD2's `get_camera_frame_size` |
| `SensorName` | `QHY5III715C-6cf42c41a8b2f3188`, the name PHD2's `get_current_equipment` reports |
| `SensorType` | 0, `Monochrome` |
| `PixelSizeX` / `PixelSizeY` | 1.45 / 1.45, from the config |
| `MaxADU` | 65535 |

`SensorType = Monochrome` is right for this colour camera. PHD2's QHY
driver reports Bayer type 1 for it (debug log, `cam reports bayer type 1`),
and a single frame is captured with PHD2's `CAPTURE_LIGHT` options, which
include `CAPTURE_RECON`. With those, the QHY driver reconstructs luminance
from the Bayer frame at bin 1 (`QuickLRecon`, `src/cam_qhy.cpp`). The FITS
PHD2 writes has no `BAYERPAT` card. Over the central 512 × 512 pixels,
lit by the room at 12 000 to 12 800 ADU, its four 2 × 2 phase means agree
within 5 ADU (`probe/phases.py`), where a raw Bayer frame would show its
colour phases apart:

| Frame | Phase means (row 0: even, odd column; row 1: even, odd) |
|---|---|
| 100 ms | 12058.7, 12056.6 / 12053.6, 12051.4 |
| 1 s | 12514.9, 12515.0 / 12518.5, 12518.6 |
| 5 s | 12795.2, 12793.5 / 12798.2, 12796.7 |

The file is 16 943 040 bytes: one header block and 3864 × 2192 16-bit
pixels (`BITPIX = 16`, `BZERO = 32768`). The camera's 12-bit samples
arrive scaled to 16 bits (maximum 65520). PHD2's header also carries
`XPIXSZ`/`YPIXSZ` 1.45, `PIXSCALE` 0.83″ (from the profile's focal
length), and `GAIN 95`, PHD2's percentage (the SDK was set to 228 of
0–240).

## Timing

All times come from the probe, which ran on the rig, drove the facade over
loopback and read PHD2's events as a second event-server client
(`probe/qhyprobe.py`). `ImageReady` was polled every 10 ms.

**PHD2 alone** (`capture_single_frame` with a path), from the request to
`SingleFrameComplete`:

| Exposure | PHD2's reply | `SingleFrameComplete` | Beyond the exposure |
|---|---|---|---|
| 100 ms | 43 ms | 0.649 s | 0.55 s |
| 1 s | 70 ms | 1.524 s | 0.52 s |
| 5 s | 70 ms | 5.547 s | 0.55 s |

The half second beyond the exposure is PHD2 reading out the camera,
reconstructing luminance and writing 17 MB to disk. The simulator, with a
752 × 580 frame, needed under 0.1 s.

**Through the facade**, from `StartExposure` sent:

| Exposure | `StartExposure` answered | PHD2's event | `ImageReady` | `ImageArray` (ImageBytes, loopback) |
|---|---|---|---|---|
| 0.1 s | 48 ms | 0.598 s | 0.700 s | 0.208 s, 33 879 596 bytes |
| 1 s | 47 ms | 1.496 s | 1.600 s | 0.213 s |
| 5 s | 45 ms | 5.529 s | 5.631 s | 0.214 s |

From PHD2's event to `ImageReady` took 0.102 s each time: the facade
reading, decoding and publishing the frame. The facade sends `ImageArray`
as 32-bit integers (ImageBytes transmission type 2), so a frame is 34 MB
on the wire. On this host a client on loopback has a frame 0.8 s after
the exposure ends: 0.6 s to `ImageReady` and 0.2 s to download it.

## Aborting: PHD2's QHY driver lets the frame run

PHD2 2.6.14 builds its QHY driver without `CAN_STOP_CAPTURE`. The
`#define` is commented out in `src/cam_qhy.cpp` ("stopping capture causes
problems on some cameras on Windows, disable it for now"). The exposure
then waits in `WorkerThread::MilliSleep` with its default interrupt mask,
`INT_TERMINATE`, which a stop does not wake. So `stop_capture` changes
nothing for a QHY camera:

- **PHD2 alone.** `stop_capture` sent 1.0 s into a 5 s frame was answered
  `0` at once. `SingleFrameComplete` came 5.52 s after the request with
  **`Success: true`**, and the full frame was saved. The simulator
  answered the same stop with `Success: false` and no file 65 ms later.
- **Through the facade** (`probe/probe-facade-abort.jsonl`, `abort`).
  `AbortExposure` sent 1.0 s into a 5 s exposure answered after 2.003 s.
  The facade's 2 s wait for PHD2's event ran out (`service.log`: "PHD2 has
  not reported the aborted frame yet"). `CameraState` read `Exposing` (2)
  until PHD2's event, 5.48 s after the start, with `Success: true`. The
  facade then deleted the file and ended the exposure as aborted
  ("aborted exposure ended"). `ImageReady` stayed false, and the next
  `StartExposure` was accepted.
- **A start sent right after the abort answered**
  (`probe/probe-abort-then-start.jsonl`) was refused with `1035`
  (`InvalidOperation`), "an exposure is already in flight". The camera
  read `Exposing` until the frame's natural end, then `Idle`, and the
  next start succeeded.

This is the behaviour [phd2-guider.md](../../services/phd2-guider.md#aborting)
documents for a camera driver that ignores the interrupt. A QHY guide
camera behind PHD2 cannot end an exposure early. A client that aborts
gets its answer after 2 s, and the camera stays busy until the frame's
own end.

## Deployment finding: PHD2 could not open the camera

The first `set_connected` failed with "equipment failed to connect:
camera". PHD2's debug log said `QHY: found 0 cameras`. The camera's device
node was `crw-rw---- root:rusty-photon`. The packaged
`90-rusty-photon-qhy.rules` gives every device with QHY's vendor id to the
`rusty-photon` group with mode 0660, overriding QHY's own
`85-qhyccd.rules` (mode 0666). The operator's account, which PHD2 runs as,
is not in that group. [packaging.md](../../packaging.md#phd2-guider-the-camera-facade)'s
facade recipe does not mention this. The same holds for any guide camera
whose vendor has a packaged driver on the host, since every camera
package's rule covers its whole vendor id.

For the run, the operator's account was given read and write on that one
device node with `setfacl` (it does not outlive an unplug or a reboot),
and the ACL was removed afterwards. Granting access was not enough on its
own. PHD2's QHY SDK kept answering "0 cameras" without scanning again, and
only a PHD2 restart made it find the camera.


**The fix, run the same day.** A udev rule keyed on the camera's port gives
that one port to the operator's group, and is now step 1 of
[packaging.md](../../packaging.md#phd2-guider-the-camera-facade)'s facade
recipe:

```
SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ENV{ID_PATH_WITH_USB_REVISION}=="platform-xhci-hcd.1-usbv3-0:1", GROUP="<operator's group>"
```

In `/etc/udev/rules.d/91-rusty-photon-phd2-guide-camera.rules`, after
`udevadm control --reload` and a `change` trigger for the device, the node
read `crw-rw---- root:<operator's group>`, and a second `change` trigger
left it so. The camera's product id stayed `0716` across every trigger:
`85-qhyccd.rules` loads firmware only on `add`. A PHD2 started afterwards
connected the camera and saved a 100 ms frame with no ACL
(`SingleFrameComplete`, `Success: true`, 0.644 s). The rule was then
deleted, and a `change` trigger returned the node to
`root:rusty-photon`. The installed `qhy-camera` predated `doctor
--devices`, so the port was read from `udevadm info`; it is the spelling
`doctor --devices` prints on this host.

## Service log

`service.log` is the facade's output for the whole session at
`RUST_LOG=debug`. It starts with two lines from `cargo run`. Of its 284
`WARN`/`ERROR` lines, every one has an `ascom_alpaca::server` target, and
none comes from the facade's own modules:

| Count | Kind |
|---|---|
| 148 | `WARN` Alpaca method is not implemented: the optional members the facade does not implement |
| 112 | `WARN` Unused parameters: ConformU's protocol probes |
| 10 | `INVALID_OPERATION` no exposure has started yet: `LastExposureDuration`/`LastExposureStartTime` read before any exposure |
| 4 | `INVALID_VALUE` dark frames are not supported |
| 4 | `INVALID_VALUE` a bin other than 1 |
| 4 | `INVALID_VALUE` a subframe that does not fit the sensor |
| 1 | `INVALID_VALUE` a negative `Duration` |
| 1 | `INVALID_OPERATION` an exposure is already in flight: the probe's start right after an abort |

## Privacy scrub

`service.log` and the files in `probe/` had the rig's hostname replaced
with `<rig-host>` and the operator's home directory with `<home>`. In
`service.log` that is each PHD2 event's `Host` field and the frame paths.
In the probe outputs it is each event's `Host` field and the frame paths.
`service.log` also had its terminal colour codes removed. The ConformU
files (`alpacaprotocol.log`, `conformance.log`, `conformance-results.json`)
are unmodified.

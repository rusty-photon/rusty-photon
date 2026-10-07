# qhy-camera on Linux: QHY178M + CFW, 2026-10-05 (a camera that leaves the bus)

This is a recorded Linux ConformU run against the same physical QHY178M + CFW as the
[2026-09-27 record](../2026-09-27-qhy-camera-qhy178m-cfw-linux/README.md). It was
taken on the change that makes a camera that has left the USB bus read
disconnected ([C9 / FW4](../../services/qhy-camera.md#enumeration--connection-lifecycle),
issue #1329, PR #1405).

Next to the ConformU run are the hardware probes the change exists for:

- a 12 V cut that leaves the camera on USB (the negative control);
- a USB cable pull with the camera idle, and the reconnect after it;
- a cable pull in the middle of a 30 s exposure.

## What was tested

| | |
|---|---|
| Commit | [`939b901d`](https://github.com/rusty-photon/rusty-photon/commit/939b901d) (branch `fix/qhy-camera-camera-left-bus-1329`, PR #1405), on `main` at `a540fd1b` |
| Service | `qhy-camera`, **real-SDK** build (default features), `cargo run -p qhy-camera -- --log-level debug` (dev profile); rustc 1.99.0 (b940084d7 2026-09-28) |
| SDK | QHYCCD SDK **26.06.04**: `/usr/local/lib/libqhyccd.so` → `libqhyccd.so.26.6.4.16`, sha256 `f51b92f9189fae7707e98ad334cf52d3c1493a6485f33394b39a18a3f4d5c738` (byte-identical to every earlier QHY record) |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.8-200.fc44 |
| Camera | QHY178M-Cool (USB `1618:c179`), SDK id `QHY178M-222b16468c5966524`, with its 7-slot CFW. It sits on USB3 port 3 of the Pegasus PPBA Gen2's embedded hub, and its 12 V comes from the PPBA's Quad 12V output, driven through `ppba-driver` (`cargo run -p ppba-driver`) |
| ConformU | **4.5.0** build 53834.49ab847, against `http://127.0.0.1:11121/api/v1/{camera,filterwheel}/0` |

The service was started with the Quad 12V output already on. The CFW still enumerated
late (`cameras=1 filter_wheels=0`), so a reload (SIGHUP) re-enumerated it
(`filter_wheels=1`) before anything below ran. C0 describes the same behaviour.

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

C9 puts the presence check after every capability probe, so every `HasShutter`,
`CanSetCCDTemperature` and `SensorType` ConformU read also asked whether the camera
was still there. Across both suites — every connect, disconnect, exposure and error
case — the service logged no departure and no `WARN` or `ERROR` of its own.

## The probes

The probes are Alpaca requests made with a small client. Each row is one exchange,
with times in local time (UTC−7); the service log is in UTC. Each phase asserted its
precondition with `lsusb` first: the camera on the bus for the negative control,
and off the bus for the departures. The cable was pulled and replaced by hand.

The PPBA's `USB Hub` switch (`PU:0`, the only USB control `ppba-driver` exposes) was
tried first and **did not take the camera off the bus**. The camera is on one of the
hub's USB3 ports, and `PU` switches only its USB2 side. The switch was set back on.

### 1. Negative control: 12 V cut, camera still on USB

The QHY178M's logic runs off USB, so cutting the Quad 12V removes only the TEC's and
the wheel's power. C9 must **not** call this camera gone.

```
08:58:16  PUT setswitch (Quad 12V off)
08:58:19  GET ccdtemperature           value=24.4           err=0x0
08:58:20  GET ccdtemperature           value=24.7           err=0x0
08:58:21  GET ccdtemperature           value=24.9           err=0x0
08:58:22  GET coolerpower              value=0.0            err=0x0
08:58:22  GET cooleron                 value=False          err=0x0
08:58:22  GET cansetccdtemperature     value=True           err=0x0
08:58:22  GET hasshutter               value=False          err=0x0
08:58:22  GET sensortype               value=0              err=0x0
08:58:22  PUT startexposure            value=None           err=0x0  (0.01 s)
08:58:24  GET imageready               value=True           err=0x0
08:58:24  GET connected (camera)       value=True
08:58:24  GET connected (wheel)        value=True
{"camera_connected": true, "wheel_connected": true, "departures_logged": 0}
```

With its 12 V cut the camera answered every SDK call, and reported its cooler as
present. The three probes ran the presence check against a camera that was degraded
but present, and neither device was marked lost. The 12 V was restored afterwards.

### 2. Cable pulled with the camera idle

```
--- lazy window: nothing has reached the SDK yet ---
16:32:07  GET connected                value=True           err=0x0
16:32:07  GET gain                     value=51             err=0x0  (cache-served)
--- first SDK call ---
16:32:07  GET ccdtemperature           value=None           err=0x407 NOT_CONNECTED
--- after it ---
16:32:07  GET connected                value=False          err=0x0
16:32:07  GET gain                     value=None           err=0x407 NOT_CONNECTED
16:32:07  GET camerastate              value=None           err=0x407 NOT_CONNECTED
16:32:07  GET cansetccdtemperature     value=None           err=0x407 NOT_CONNECTED
16:32:07  GET cameraxsize              value=None           err=0x407 NOT_CONNECTED
16:32:07  GET connected (wheel)        value=False          err=0x0
16:32:07  GET position (wheel)         value=None           err=0x407 NOT_CONNECTED
--- release: Connected=false on both, the camera still gone ---
16:32:07  PUT connected (camera)       value=None           err=0x0  (0.5 ms)
16:32:07  PUT connected (wheel)        value=None           err=0x0  (0.5 ms)
--- reconnect while the camera is still gone ---
16:32:07  PUT connected                value=None           err=0x407 NOT_CONNECTED
16:32:07  GET connected                value=False          err=0x0
```

The service logged a single `WARN` for the departure:

```
23:32:07.730188Z  WARN qhy_camera::backend: camera no longer answers for a control its connect required;
                  it has left the bus, and every device on its connection now reads disconnected
```

The probe's own summary line printed `"departures_logged": 2`. That figure is wrong:
the counter matched on the phrase "has left the bus", which the request's `DEBUG`
line repeats. The log has exactly one `WARN`. The release produced no close error,
and the refused reconnect was the SDK's own: `qhyccd_rs::camera: error=Sdk { op: "open_camera" }`.

**This answers half of what C9 owed to hardware.** On Linux, with SDK 26.06.04,
`IsQHYCCDControlAvailable` stops answering for `CamSingleFrameMode` once the camera
has been unplugged. `CCDTemperature` reached the SDK first, and its own `Cooler`
probe failed as well.

### 3. Cable back in: a plain reconnect

```
on USB: ['Bus 002 Device 005: ID 1618:c179 QHYCCD Q178-Cool']
16:33:31  PUT connected                value=None           err=0x0  (301.9 ms)
16:33:31  GET connected                value=True           err=0x0
16:33:31  GET ccdtemperature           value=25.3           err=0x0
16:33:31  PUT startexposure            value=None           err=0x0  (0.01 s)
16:33:34  GET imageready               value=True           err=0x0
```

The camera came back as a new USB device (`003` → `005`). `OpenQHYCCD` still found
it by its id, with no re-scan and no reload, and the fresh open cleared the lost
mark. The wheel reconnected onto the same new handle (`Position` 0, seven names).
C9 had called this unmeasured.

### 4. Cable pulled mid-exposure

A 30 s light frame was started, and the cable was pulled while it ran. The watcher
read only `Connected` and `CameraState`, neither of which reaches the SDK, so any
change it saw came from the capture task, not from the watcher.

```
16:34:31  PUT startexposure  (30 s)    err=0x0
16:34:31  GET camerastate              value=2 (Exposing)
t+  14.0s  Connected=True  CameraState=2
t+  32.0s  Connected=False CameraState=None err=0x407
{"warnings_logged": 1, "final_connected": false}
```

```
23:35:02.719439Z ERROR qhyccd_rs::camera: error=Sdk { op: "get_remaining_exposure_us" }
23:35:02.719553Z ERROR qhyccd_rs::camera: error=Sdk { op: "get_image_size" }
23:35:02.719566Z DEBUG qhyccd_rs::camera: control=IsControlAvailable { control: CamSingleFrameMode }
23:35:02.719574Z  WARN qhy_camera::backend: camera no longer answers for a control its connect required; ...
23:35:02.719643Z  WARN qhy_camera::camera: mid-exposure SDK error error=QHYCCD SDK operation 'get_image_size' failed
```

The capture slept through its exposure, as it is built to. When the frame was due,
the progress poll and the readout's size query both failed and nothing hung. The
capture's own presence check then marked the connection lost. That happened without
any client call, about 31 s after the start rather than when the cable came out.

`get_image_size` refused first, so the readout never entered `GetQHYCCDSingleFrame`
on the departed camera. Whether *that* call would return or hang is still unmeasured.

After replugging, the first reconnect was **refused** by design (FW4):

```
16:35:47  PUT connected (camera)       err=0x407 NOT_CONNECTED
23:35:47.620340Z  WARN qhy_camera::backend: connect refused: the camera has left the bus and another
                  device on its connection still holds it; disconnect every device on it first
```

This time the wheel had not been released, and it still held the lost handle. Once
`Connected = false` was sent to the wheel, both devices reconnected, read back the
cooler (`CanSetCCDTemperature` true, 23.6 °C), gain 30 and slot 0, and took a frame.

## Not compared against `main`

No A/B against `main` was made, because on `main` the result is fixed by
construction. There `Connected` reads a flag that only the driver's own connect and
disconnect write, so no cable pull can change it; issue #1329 records that behaviour
on rig2's QHY600M. What this run had to establish was the SDK side: whether the
probe C9 relies on fails once the camera has gone. Section 2 shows that it does.

## Still owed

- **Windows.** The same cable pull or 12 V cut on rig2's QHY600M.
- **A camera that leaves while its frame is being read out.** Section 4 shows the
  SDK refusing *before* the readout; a departure *inside* `GetQHYCCDSingleFrame` was
  not reached.

## After the run

Both devices were disconnected and both services stopped. The Quad 12V output was
turned off again, the box's between-sessions state, and the PPBA's `USB Hub` switch
was left on.

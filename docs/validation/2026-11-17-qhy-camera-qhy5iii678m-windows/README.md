# qhy-camera on Windows — QHY5III678M on rig2, 2026-11-17

This is the first record of rig2's guide camera, the QHY5III678M, under
`qhy-camera`. Until this run the driver's `usb_devices` list on rig2 named only
the imaging camera, a QHY600M with its wheel, and kept the guide camera out of
the driver so that PHD2 could open it through its own SDK. The run added the
guide camera to the list at the next device number, 1. The QHY600M stayed at 0,
the number `rp`'s configuration gives it. The run then took ConformU through the
guide camera, and the cadence spike that stage G1 of the
[native guider plan](../../plans/native-guider.md) asks of each guide camera.
The guide camera stays under `qhy-camera` after the run.

- **ConformU 4.5.3:** `alpacaprotocol` and `conformance` both clean.
  `conformance-results.json`'s `ErrorCount`, `IssueCount`,
  `ConfigurationAlertCount` and `TimingIssuesCount` are all 0.
- **Cadence:** 2 s exposures ran back to back through the production endpoint
  for an hour, full frame binned 2 × 2. Over 1446 frames, with no failures, the
  client had the pixels 489 ms after the exposure ended (median). The 95th
  percentile was 521 ms and the slowest frame 568 ms, and the medians held to
  within 5 ms from the first ten minutes to the last. **Run A meets stage G1's
  pass criterion.** A 512 × 512 subframe took 311 ms and an unbinned frame
  546 ms (medians).

Dates and times here are UTC unless marked otherwise. The run took place
between 03:42 and 05:23 UTC on 2026-11-18, which was the evening of
2026-11-17 at the rig (US Central time, UTC−6). ConformU's logs stamp the
rig's local time.

## What was tested

| | |
|---|---|
| Commit | [`21bfe94d`](https://github.com/rusty-photon/rusty-photon/commit/21bfe94d) (`main`), published by the nightly run [42369066171](https://github.com/rusty-photon/rusty-photon/actions/runs/42369066171) as `0.1.0+nightly.202611071139.g21bfe94` |
| Service | `qhy-camera` from the suite MSI `rusty-photon-0.1.0+nightly.202611071139.g21bfe94-x64.msi` (sha256 `f43dab80016481850c85f0945e857a2a58c29aa7d78c41eed6f0cd897ef5667c`, matching the release's `SHA256SUMS.txt`), installed on 2026-11-07 as `0.1.0.26311`. The installed `rusty-photon-qhy-camera.exe` has sha256 `2c3f3b068f40274e88beb21c3b81b5c95b824899dd44a3d8ecd7d8bdaf5f1231` and reports `qhy-camera 0.1.0` |
| How it ran | For ConformU, the installed service was stopped, as for the [QHY600M's record](../2026-10-03-qhy-camera-qhy600m-cfw-windows/README.md). The **same installed binary** then ran in console mode on plain HTTP at `127.0.0.1:11141`, with the production `usb_devices` list and no `tls` or `auth`, and the service was started again afterwards. The cadence runs used the **installed service** on its production endpoint, `https://127.0.0.1:11121`, with TLS and the observatory credential. The script read the credential from `rp.json`'s entry for the service when it ran |
| SDK | `qhyccd.dll` **24.1.9.12** from the QHY All-in-One (`C:\Program Files\QHYCCD\AllInOne\sdk\x64`, the copy the service loads), sha256 `f99916434e3734372a051cc791f4629d7d6801ee7077d48e3315af1f848194ea`; camera driver `oem62.inf` 23.12.26.0 |
| Platform | Windows 11 Pro 25H2 x64 (build 26200.9974), Intel Core i3-1220P (12 logical processors) |
| Camera | QHY5III678M. SDK id and ASCOM `UniqueID`: `QHY5III678M-4a94be45b27d3aa79`. Bus record `1618:0679`, bus-reported description `QHY678U3G20-20230106`, no USB serial. Port `PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(3)`, the UPBv2 hub's USB 3. Sensor 3856 × 2180 with 2.0 µm pixels, `SensorType` monochrome, `MaxBinX` 2, gain `[0, 240]`, offset `[0, 1023]`, `MaxADU` 65535. One readout mode, `STANDARD MODE`. Seeded at connect at gain 30 and offset 30. No cooler, no shutter |
| ConformU | **4.5.3** build 56334.33d9e18, the latest release. It was installed on the rig for this run over 4.5.0, from the release's installer (sha256 matching the release, Authenticode-signed by Peter Simpson). It ran there against `http://127.0.0.1:11141/api/v1/camera/1`, with a settings file holding only `{"SettingsCompatibilityVersion": 1}` |

## The handover

These are the steps as run, by which the camera changed hands. Before each
step, no session was running on the rig: no NINA, PHD2 or session-runner
process, no capture in `rp`'s log, and no power on the imaging train. A watcher
repeated that check every three minutes through the cadence runs, and would
have stopped them if a session had started.

1. **As found.** PHD2 was not running. The rig was in its state between
   nights. The UPBv2's 12 V output 1 (the QHY600M) and its USB 3 (the guide
   camera) were off, so neither camera was on the bus. `qhy-camera` had been
   started at 2026-11-13 02:11 with that power off, and served its listed
   camera 0 and wheel 0 as placeholders (`cameras=0 filter_wheels=0
   placeholders=2`). `rp`'s `GET /api/equipment` read its camera `qhy600m`
   disconnected.
2. **Power.** Pegasus Unity was not running, so `upbv2-driver` was started
   and connected, and every switch was read and saved. USB 3 was switched on
   at 03:47:13, and the driver was disconnected and stopped again. The camera
   enumerated as `1618:0679`.
3. **The camera listed.** At 03:47:34, `rusty-photon-qhy-camera doctor
   --devices --config C:\ProgramData\rusty-photon\qhy-camera.json` listed the
   camera without opening it:

   ```text
   QHY cameras on the bus (vendor 1618), in port order:

     Port                                            Model        SDK id                         USB serial  Device
     PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(3)  QHY5III678M  QHY5III678M-4a94be45b27d3aa79  —           not listed
   ```

   Its `usb_devices` block repeated the configured list, which held the
   QHY600M alone. It also warned that device 0 would be a placeholder, since
   that camera had no power.
4. **The list.** `qhy-camera.json` was copied to
   `qhy-camera.json.bak-20261117-g1` beside it. An entry for the guide camera
   was added, with its port pasted from the doctor's table:

   ```json
   "usb_devices": [
     {
       "device_number": 0,
       "usb_port": "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)",
       "name": "QHY600M",
       "description": "QHY600M mono + 7-position CFW, imaging camera on the main train",
       "filter_wheel_number": 0,
       "filter_names": ["L", "R", "G", "B", "H", "S", "O"]
     },
     {
       "device_number": 1,
       "usb_port": "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(3)",
       "name": "QHY5III678M",
       "description": "QHY5III678M mono guide camera"
     }
   ]
   ```

   Run again against the edited file, the doctor put the guide camera at
   device 1 and printed both entries as its block.
5. **Restart.** `rusty-photon-qhy-camera` was restarted at 03:48:38. Its log
   had the placeholder warning for device 0 that every start with the QHY600M
   unpowered logs. Then came `Service started successfully on 0.0.0.0:11121
   cameras=1 filter_wheels=0 placeholders=2`, and nothing else new.
   `/management/v1/configureddevices` listed:

   | Type | Number | Name | `UniqueID` |
   |---|---|---|---|
   | Camera | 0 | QHY600M | `placeholder:qhy-camera:PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)` |
   | Camera | 1 | QHY5III678M | `QHY5III678M-4a94be45b27d3aa79` |
   | FilterWheel | 0 | QHY filter wheel on PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1) (placeholder) | `placeholder:qhy-camera:CFW-PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)` |

   The imaging camera kept its number, 0. It was still a placeholder, as
   before the change, because its power was off. `rp` reached the service as
   before. Its `GET /api/equipment` answered with `qhy600m` disconnected, and
   its polls of wheel 0 went on reaching the placeholder.
6. Nothing failed, so the backup was not needed.

From then until the end of the run, the service log's one new line was a
`NOT_CONNECTED` answer at 03:55:05.702. It came 19 ms after `rp` logged
`conditions unsafe; cancelling in-flight work that needs the sky`, when its
safety monitor turned unsafe, and `rp` addresses camera 0 alone. Neither the
roof nor the sky matters to these runs.

## ConformU verdicts

| `alpacaprotocol` | `conformance` |
|---|---|
| 0 errors, 0 issues, 16 information messages — [log](alpacaprotocol.log) | *"no errors, warnings or issues found"*; every member within its target response time — [log](conformance.log), [results](conformance-results.json) |

ConformU ran from 03:52:13 to 03:52:37. The 16 informational items are the set
every QHY record carries: four casing variants each against `ImageArray`,
`ImageArrayVariant`, `LastExposureDuration` and `LastExposureStartTime`, before
any exposure exists.

ConformU took a 2 s full frame at each bin the camera offers: 3856 × 2180 at
bin 1 and 1928 × 1090 at bin 2. `StartExposure` answered in 37 ms and 35 ms.
`ImageReady` read true 2.53 s after each `StartExposure` returned, as ConformU
polled it. The 32-bit `ImageArray` read in 117 ms and 123 ms. ConformU's gain
and offset checks wrote each minimum and maximum, and were refused
`InvalidValue` one below and one above. The console process held those last
values and ended with the run. The service's next connect seeded gain 30 and
offset 30 again, and the cadence script read those values.

## The cadence runs

### Method

[`cadence/cadence.ps1`](cadence/cadence.ps1) ran on the rig as a one-shot
`SYSTEM` scheduled task, so it did not depend on the ssh session. It is a
PowerShell script using .NET's `HttpClient` and the `Stopwatch` clock, and it
talked to the installed service through the standard Alpaca Camera API, as any
client would. Every frame went through these steps:

1. `t_req` was stamped just before `PUT startexposure` (`Duration=2`,
   `Light=true`), and `t_started` just after its answer.
2. `GET imageready` was polled every 25 ms until it read true, at `t_ready`.
3. `GET imagearray` was requested with `Accept: application/imagebytes`, and
   `t_pixels` was stamped when the whole body had been read.
4. The next frame's `t_req` followed at once.

From these the script derived, per frame:

- `start_call` = `t_started − t_req`;
- `ready_after_end` = `t_ready − (t_req + 2 s)`;
- `download` = `t_pixels − t_ready`;
- `latency` = `t_pixels − (t_req + 2 s)`;
- `cycle` = `t_pixels − t_req`.

`latency` is the "exposure end to pixels in the client" of the pass criterion.
Measured from the request, it includes the `StartExposure` call, so it can only
overstate.

The frame was set before each run, and gain (30), offset (30) and readout
mode (0) were left at the camera's values. The script checked every
ImageBytes body's error field and dimensions. The host's CPU load was sampled
once a minute: the processor-time counter's average over the minute, and
`qhy-camera`'s share of the host.

| Run | Frame | Started | Length |
|---|---|---|---|
| A | full frame, bin 2 × 2: 1928 × 1090 (4 203 084 bytes) | 03:59:49 | 60 min |
| B | 512 × 512 at the sensor's centre, `StartX` 1672, `StartY` 834, bin 1 (524 332 bytes) | 04:59:52 | 10 min |
| C | full frame, bin 1: 3856 × 2180 (16 812 204 bytes) | 05:09:54 | 10 min |

Before them, a one-minute trial of each run checked the script from 03:55:50
to 03:58:55. Its rows are not in this record.

The record keeps each run's frames (`run-A.csv`, `run-B.csv`, `run-C.csv`),
its summary (`summary-*.json`), its CPU samples (`cpu-*.csv`), the camera's
properties as read before run A (`camera.json`) and the script's own log
(`status.txt`). The script's copy here differs from the one that ran in one
line, line 19, the output directory's default. There the local user name is
replaced by `<rig-user>`.

### Summaries

| | A: bin 2, 60 min | B: 512 × 512, 10 min | C: bin 1, 10 min |
|---|---|---|---|
| Frames | 1446 | 260 | 236 |
| Failures | 0 | 0 | 0 |
| Frames per hour | 1445 | 1554 | 1412 |
| `latency_ms` median / p95 / max | **489.1 / 520.8 / 568.0** | 310.8 / 343.1 / 359.5 | 546.0 / 585.6 / 621.1 |
| `ready_after_end_ms` median / p95 / max | 459.6 / 486.3 / 519.1 | 304.9 / 334.7 / 347.4 | 462.3 / 489.1 / 494.4 |
| `download_ms` median / p95 / max | 28.8 / 48.8 / 70.6 | 4.2 / 15.4 / 25.9 | 78.9 / 120.3 / 140.1 |
| `cycle_ms` median / p95 / max | 2489.1 / 2520.8 / 2568.0 | 2310.8 / 2343.1 / 2359.5 | 2546.0 / 2585.6 / 2621.1 |
| `start_call_ms` median / p95 / max | 36.0 / 36.8 / 49.0 | 35.8 / 36.7 / 43.8 | 36.1 / 36.7 / 36.9 |
| Latency over 500 ms | 368 (25.45 %) | 0 | 236 (100 %) |
| Latency over 1000 ms | 0 | 0 | 0 |

Run A's six ten-minute blocks have latency medians of 486.1, 489.2, 489.0,
489.1, 489.4 and 490.9 ms, and 95th percentiles of 518.5 to 525.1 ms. Each
run's first frame, the first after a frame change, was within the run's range:
518, 325 and 562 ms.

During run A the host's CPU load read 0.6–2.8 % (median 2.0 %) over 57
one-minute samples. `qhy-camera` took 0.33 % of the host at the median, about
4 % of one logical processor, and held a 62–63 MB working set. It took
0.24–0.27 % of the host in run B and 0.42–0.50 % in run C, with 56 MB and 87 MB.

### Against the pass criterion

Stage G1 passes when the pixels reach the client well under the exposure
length after the exposure ends, sustained for an hour. Run A did that. Its
slowest frame of 1446 reached the client 568 ms after the 2 s exposure
ended, which is 28 % of the exposure, and the median and the tail held still
for the whole hour. No frame failed. That is the half second the plan's
§ 7 calls tolerable on a 2 s exposure.

### Where the time goes

Almost all of the latency comes before `ImageReady` reads true:

- **`ready_after_end`** is about 460 ms for a full frame at either bin, and
  305 ms for the subframe. The 25 ms poll adds about 12 ms of that on
  average.
- **`download`**, the ImageBytes body over loopback, is 29 ms for 4.2 MB at
  bin 2 and 79 ms for 16.8 MB at bin 1.
- **`StartExposure`**, during which the exposure is armed and begins, takes
  36 ms. Measured from its answer rather than its request, run A's latency
  is 453 ms (median).

The driver logs at `debug` in console mode, and its log during the ConformU run
places the time spent before `ImageReady` inside the SDK. After it logged a
frame's gain and offset armed, the frame came back from the SDK (`frame read`)
2.431 s later at bin 1 and 2.459 s later at bin 2: 431 ms and 459 ms after the
2 s exposure. That span is the driver's exposure-remaining poll and
`GetQHYCCDSingleFrame`, which the log does not split.

The readout costs the same at bin 1 and bin 2, and less for a 512-row
subframe. That fits a readout that follows the region read off the sensor
rather than the binned frame's size.

ConformU's `ImageReady` at 2.53 s puts this camera and build at the low end
of the 2.5–4.5 s that single-shot conformance runs have shown.

## After the run

At 05:22:30, with the cadence script finished and the camera disconnected by
it, USB 3 was switched off again through `upbv2-driver`. Every UPBv2 switch
then read as found: 12 V output 3 and USB 1, 2 and 6 on, the rest off.
`upbv2-driver` was disconnected and stopped. `qhy-camera` was restarted at
05:22:37, so that it holds no camera that has gone. It now serves both listed
cameras and the wheel as placeholders: `cameras=0 filter_wheels=0
placeholders=3`, with device 1 naming `…#USB(14)#USB(3)`. Every service is in
the state it was found in. `rp` reads `qhy600m` disconnected, as before.

What the run leaves changed on the rig:

- `usb_devices` holds the guide camera at device 1, beside the QHY600M at 0.
- `qhy-camera.json.bak-20261117-g1` sits beside it, holding the list as found.
- ConformU 4.5.3 replaced 4.5.0.

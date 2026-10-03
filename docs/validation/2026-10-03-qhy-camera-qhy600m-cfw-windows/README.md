# qhy-camera on Windows — QHY600M + CFW on rig2, 2026-10-03

Recorded Windows ConformU run against rig2's QHY600M and the CFW on its port —
the first record for this camera, and the first taken on rig2, the remote
Windows observatory rig. Taken on the published nightly that carries every change to this
driver since the rig's previous build, among them the readout-mode
re-initialization (RM1), the bin cached at the setter and armed by
`StartExposure` (B1), and `Gain` and `Offset` cached at the setter and armed
by `StartExposure` (GO1/GO2). Beside it, the hardware checks those three
rules owed a multi-mode camera: a set made **during** an exposure, against the
rig's previous build and this one; gain and offset carried across a
readout-mode change; and a bin shown by hot pixels.

Dates and times here are UTC: the run took place between 00:54 and 01:31 UTC
on 2026-10-03, the evening of 2026-10-02 at the rig.

## What was tested

| | |
|---|---|
| Commit | [`689f8512`](https://github.com/rusty-photon/rusty-photon/commit/689f8512) — `main`, published as nightly `0.1.0+nightly.202610030048.g689f851` ([run 37083558259](https://github.com/rusty-photon/rusty-photon/actions/runs/37083558259)) |
| A/B baseline | The rig's previous build, nightly `0.1.0+nightly.202609240953.g1b2a6c3` — `main` at [`1b2a6c3a`](https://github.com/rusty-photon/rusty-photon/commit/1b2a6c3a), before the bin, gain and offset caching |
| Service | `qhy-camera` from the suite MSI `rusty-photon-0.1.0+nightly.202610030048.g689f851-x64.msi` (sha256 `af1d3c4a63cf157284d91d5fe39ba20495c55d73a5c204f025471f31a3cbdd1a`, matching the release's `SHA256SUMS.txt`), installed over the previous nightly with `msiexec /i … /qn /norestart`; the installed `rusty-photon-qhy-camera.exe` (sha256 `fb2914b76c2d3a764b80abd026c0aca479e0cf6cfd627e02677cf9d85f1460da`) built by the nightly workflow with rustc 1.99.0 (b940084d7 2026-09-28) |
| How it ran | The installed service answers over TLS with credentials, so for the run it was stopped and the **same installed binary** was started in console mode with a plain-HTTP configuration bound to `127.0.0.1:11141` (`"devices": {}`, so it enumerates every QHY camera present); the service was started again afterwards |
| SDK | `qhyccd.dll` **24.1.9.12** from the QHY All-in-One (`C:\Program Files\QHYCCD\AllInOne\sdk\x64`, resolved through `PATH`, the copy the service loads), sha256 `f99916434e3734372a051cc791f4629d7d6801ee7077d48e3315af1f848194ea`; camera driver `oem62.inf` 23.12.26.0 |
| Platform | Windows 11 Pro 25H2 x64 (build 26200.9457), Intel Core i3-1220P |
| Camera | QHY600M, firmware 2023-06-14 (`QHY600U3G20-20230614`), 9576×6384 in mode 0, mono, gain `[0, 200]`, offset `[0, 255]`, ten readout modes listed by this SDK — SDK id `QHY600M-3b1ea54688eb9f9d7`; seeded at connect at gain 30, offset 30. Camera 1 of two on this host (the other is a QHY5III678M, not tested) |
| FilterWheel | The CFW on that camera's port, 7 slots, same physical `OpenQHYCCD` handle — `CFW-QHY600M-3b1ea54688eb9f9d7` |
| ConformU | **4.5.0** build 53834.49ab847, installed on the rig for this run and run there against `http://127.0.0.1:11141/api/v1/camera/1` and `.../filterwheel/0`, with a settings file holding only `{"SettingsCompatibilityVersion": 1}` |

## Verdicts

Both devices, both suites, clean:

| Device | `alpacaprotocol` | `conformance` |
|---|---|---|
| Camera | 0 errors, 0 issues, 16 information messages — [log](alpacaprotocol-camera.log) | *"no errors, warnings or issues found"*; every member within its target response time — [log](conformance-camera.log), [results](conformance-camera-results.json) |
| FilterWheel | *"no errors, issues or information alerts"* — [log](alpacaprotocol-filterwheel.log) | *"no errors, warnings or issues found"*; every member within its target response time — [log](conformance-filterwheel.log), [results](conformance-filterwheel-results.json) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are **0** in both results files. The camera's 16 informational items are the
set every QHY record carries: four casing variants each against `ImageArray`,
`ImageArrayVariant`, `LastExposureDuration` and `LastExposureStartTime` before
any exposure exists.

ConformU took a 2 s full frame at each bin from 1 to 4 — 9576×6384 (61.1 MP),
4788×3192, 3192×2128 and 2394×1596 — and read the bin-1 frame as a 32-bit
`ImageArray` in 807 ms. Its gain and offset checks read the seeded values,
wrote each minimum and maximum, and were refused `InvalidValue` one below and
one above, all answered from the cache. The filter wheel moved through all
seven slots in both directions, 1.8 s from one slot to the next.

## A set during an exposure

Method: a 1024×1024 sub-frame at the sensor's centre, the camera dark (at
gain 0 and offset 25, frame A here and a 1 s frame in the readout-mode test
below read the same median, 414). Per control, one 0.1 s frame at value *a*,
then three 3 s frames: A at *a*; B started at *a*, with the value set to *b*
1.2 s after its `StartExposure` returned (about 1.35 s into the exposure by
the driver's log); C, the next frame, at *b*. Driven from the dev box through
an ssh tunnel to the console-mode driver, before the ConformU run.

| | Offset 25 → 85 (gain 0), median | Gain 0 → 100 (offset 30), median |
|---|---|---|
| previous build — A | 414 | 494 |
| previous build — B (set in flight) | **1374** | **3063** |
| previous build — C | 1374 | 3063 |
| this build — A | 414 | 494 |
| this build — B (set in flight) | **414** | **494** |
| this build — C | 1374 | 3090 |

On the previous build both land in the frame in flight, whole: this camera,
like the QHY178M, takes a gain or an offset written mid-exposure into the
frame being read. On this build neither does, and the next frame takes both.
The `PUT` itself answered in 122–125 ms on this build and 128–170 ms on the
previous one — the tunnel's round trip, about 120 ms, in both. The two extra
`SetQHYCCDParam` writes each `StartExposure` now makes are timed by the
driver's own log, which stamps the sub-frame armed and then the gain and
offset armed: 32.3 ms apart at the median, 31.8–33.1 ms over all 31 exposures
of this build's checks, against the
[QHY178M's](../2026-10-01-qhy-camera-qhy178m-gain-offset-linux/README.md)
11 ms on Linux. Through the tunnel the difference does not resolve: ten 0.1 s
frames at unchanged settings put the `StartExposure` call at a median of
160.9 ms on this build and 159.2 ms on the previous one.

## Gain and offset across a readout-mode change

Same sub-frame, 1 s frames. Mode 0 is `PhotoGraphic DSO 16BIT`, mode 1
`High Gain Mode 16BIT`; both offer gain `[0, 200]` and offset `[0, 255]`.

| Step | `Gain` / `Offset` read | Frame median |
|---|---|---|
| Mode 0 at 0 / 25, armed by a frame | 0 / 25 | 414 |
| Set 100 / 85 with no exposure after it, then switch to mode 1 | **100 / 85** | |
| First mode-1 frame | | **1456** |
| Mode 1, 0 / 25 set and armed (control) | 0 / 25 | 408 |
| Mode 1, 100 / 85 set and armed (control) | 100 / 85 | 1454 |
| Switch to mode 0, with 51 / 55 set while it runs | **51 / 55** | |
| First mode-0 frame | | **916** |
| Mode 0, 0 / 25 (control) | 0 / 25 | 414 |
| Mode 0, 51 / 55 (control) | 51 / 55 | 916 |

When the first switch began, the camera still held 0 / 25, the values the
last frame armed; 100 / 85 existed only in the cache. They survived the
switch, and the first frame in mode 1 came back at them (1456, against 1454
for the same values set in that mode and 408 for 0 / 25). The second switch
took 2.125 s at the driver, from the `ReadoutMode` request to the logged
change; the gain and offset requests reached it 0.79 s and 0.92 s in and were
answered at once, so they landed while the camera was being re-initialized,
and the values read back after it and the first frame's level are theirs.
Each `ReadoutMode` write answered the client in 2.24–2.25 s through the
tunnel, 2.12 s at the driver.

## A bin, shown by hot pixels

Full frames at the seeded gain and offset, 5 s each, dark. The bin-2 frame
came back 4788×3192 with a median of 1988, four times bin 1's 497: a summed
2×2. Sixteen isolated pixels of the bin-1 frame, each at 65534 with every
neighbour well below it, were then looked for in the bin-2 frame by the
brightest pixel of the 3×3 neighbourhood around `(x/2, y/2)`. Every one has a
saturated 65535 there — four exactly at `(x/2, y/2)`, twelve one pixel away,
in directions that vary from pixel to pixel — while the three of them that
fall inside a top-left crop of the bin-2 frame's shape read 1995–2019 at
`(x, y)` there, against that frame's median of 1988. Back at bin 1, all
sixteen are bright within a pixel of `(x, y)` again.

So the frame is binned, not cropped. The one-pixel scatter is not
explained, and it is this camera's: the same search on the QHY178M found all
twelve of its hot pixels exactly at `(x/2, y/2)`. The driver armed the bin-2
frame at chip `(12, 0)`, exactly half of bin 1's effective-area origin
`(24, 0)`, so it is not a translation offset; with saturated sums and a 3×3
search this run cannot tell the camera's own binning registration from a
second hot pixel nearby. The frames were not kept, so settling it needs a
run that keeps them.

## After the run

ConformU leaves `Gain` and `Offset` at their maxima, 200 and 255; both were
set back to the connect-time 30 and 30 and one frame armed them. The cooler
was off when the run began and is off after it. ConformU's `CoolerOn` check
switched it on and straight back off — the two writes 1 ms apart in the
driver's log — and its `SetCCDTemperature` checks then walked the setpoint
from 0 down to −273.25 °C and up to 85 °C with the cooler off; it ended by
restoring the 14.8 °C setpoint it found and `CoolerOn = false`. The sensor
read 12.0 °C before the first check and 21.7 °C after ConformU. The filter
wheel ended at slot 0, where it was found. The mode, bin and sub-frame were
left at mode 0, bin 1 and the full frame.

# qhy-camera on Linux — QHY178M, 2026-10-01 (Gain and Offset armed by `StartExposure`)

Recorded Linux ConformU run against the same physical QHY178M as the
[2026-09-27 record](../2026-09-27-qhy-camera-qhy178m-cfw-linux/README.md),
taken on the change that caches `Gain` and `Offset` at their setters and has
every `StartExposure` send them to the camera with the bin and the sub-frame
([GO1/GO2](../../services/qhy-camera.md#gain--offset--readout)). Beside it, an
A/B of what a set made **during** an exposure does to that exposure's frame,
against `main` and against the change: the reason the change exists.

The CFW's 12 V supply was off for this run, so the service enumerated the
camera alone (`filter_wheels=0`); nothing here touches the filter wheel.

## What was tested

| | |
|---|---|
| Commit | [`b519654e`](https://github.com/rusty-photon/rusty-photon/commit/b519654e) (branch `fix/camera-gain-offset-cache-1336`), on `main` at `c13e5cae` |
| A/B baseline | `main` at [`c12f5e9d`](https://github.com/rusty-photon/rusty-photon/commit/c12f5e9d) — the gain and offset code is identical at `c13e5cae` |
| Service | `qhy-camera`, **real-SDK** build (default features), `cargo run -p qhy-camera` (dev profile); rustc 1.99.0 (b940084d7 2026-09-28) |
| SDK | QHYCCD SDK **26.06.04** — `/usr/local/lib/libqhyccd.so` → `libqhyccd.so.26.6.4.16`, sha256 `f51b92f9189fae7707e98ad334cf52d3c1493a6485f33394b39a18a3f4d5c738` (byte-identical to every earlier QHY record) |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.5-200.fc44 |
| Camera | QHY178M, 3056×2048, mono, gain `[0, 51]`, offset `[0, 1023]` — SDK id `QHY178M-222b16468c5966524`; seeded at connect at gain 30, offset 0 |
| ConformU | **4.5.0** build 53834.49ab847, against `http://127.0.0.1:11141/api/v1/camera/0` |

## Verdicts

| Suite | Result |
|---|---|
| `alpacaprotocol` | 0 errors, 0 issues, 16 information messages — [log](alpacaprotocol.log) |
| `conformance` | *"no errors, warnings or issues found"*; every member within its target response time — [log](conformance.log), [results](conformance-results.json) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are all **0**. The 16 informational items are the set every QHY record
carries: four casing variants each against `ImageArray`,
`ImageArrayVariant`, `LastExposureDuration` and `LastExposureStartTime` before
any exposure exists. ConformU's gain and offset checks read the seeded values
(30, 0), write each minimum and maximum, and are refused `InvalidValue` one
below and one above — all answered from the cache.

## A set during an exposure

Method: three 3 s frames per control on the covered camera. A at value *a*;
B started at *a*, with the value set to *b* 1.2 s into its exposure; C, the
next frame, at *b*. B is read against A and C, which differ from it only in
the value under test. Statistics over every 97th pixel.

| | Offset 102 → 341 (gain 30), median / mean | Gain 0 → 25 (offset 0), mean / p99.9 |
|---|---|---|
| `main` — A | 164 / 216.1 | 5.4 / 108 |
| `main` — B (set in flight) | **1128 / 1155.9** | **25.7 / 1332** |
| `main` — C | 1116 / 1140.3 | 24.3 / 1324 |
| change — A | 168 / 218.0 | 7.5 / 108 |
| change — B (set in flight) | **220 / 261.0** | **7.5 / 108** |
| change — C | 1128 / 1147.8 | 24.0 / 1224 |

On `main` the setter reached the SDK at once, and the frame in flight came
back entirely at the new offset and the new gain. On the change it comes
back at the values it was armed with, and the next frame takes the new ones.
B's offset median (220 against A's 168) is the camera's own drift: six
identical 3 s frames at offset 102, gain 30, taken straight after, read
medians from 132 to 220 (means 188.2–258.5), with the CFW's 12 V — and so the
sensor's cooling — off. The service log shows the mechanism: the set at
1.2 s logs `cached for the next exposure … value=341`, and only the next
exposure logs `gain and offset armed … offset=Some(341)`.

## What arming costs

Ten 0.01 s frames at unchanged settings, timing the `StartExposure` call,
which on this driver arms synchronously:

| | `StartExposure` call, median (max) | `StartExposure` → `ImageReady`, median |
|---|---|---|
| `main` (bin, ROI, exposure time) | 12.5 ms (12.8) | 2529 ms |
| change (+ gain, offset) | 23.9 ms (31.3) | 2540 ms |

The two `SetQHYCCDParam` writes cost about 11 ms per exposure on this camera,
against a 2.5 s single-frame readout.

## After the run

ConformU leaves `Gain` at its maximum and `Offset` at its maximum, so both
were set back to the connect-time values (30, 0) and one frame armed them,
leaving the camera as it was found.

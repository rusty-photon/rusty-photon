# zwo-camera on Linux — ASI1600MM-Cool, 2026-10-01 (Gain and Offset armed by `StartExposure`)

Recorded Linux ConformU run against the ASI1600MM-Cool of the
[2026-08-23 three-camera record](../2026-08-23-zwo-camera-three-cameras-reconnect/README.md),
taken on the change that caches `Gain` and `Offset` at their setters and has
every exposure arm them with its region
([GO1/GO2](../../services/zwo-camera.md#gain--offset--readout)), and that
redefines `ElectronsPerADU` as the figure for the gain the camera holds — the
last exposure's (ST2). Beside it, an A/B of what a set made **during** an
exposure does to that exposure's frame, against `main` and against the change.

The camera lay sensor-down for the A/B and the ConformU run: uncovered on the
bench it saturates at 0.1 s at gain 0.

## What was tested

| | |
|---|---|
| Commit | [`b519654e`](https://github.com/rusty-photon/rusty-photon/commit/b519654e) (branch `fix/camera-gain-offset-cache-1336`), on `main` at `c13e5cae` |
| A/B baseline | `main` at [`c12f5e9d`](https://github.com/rusty-photon/rusty-photon/commit/c12f5e9d) — the gain and offset code is identical at `c13e5cae` |
| Service | `zwo-camera`, default features (the production `zwo-rs → libzwo-sys → libASICamera2.so` path), `cargo run -p zwo-camera` (dev profile); rustc 1.99.0 (b940084d7 2026-09-28) |
| SDK | `/usr/local/lib/libASICamera2.so`, sha256 `d1de4a5ab85c8cafbddfad9c593bbba515890d3adf20c1ca44dafcf15f2775ce` |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.5-200.fc44 |
| Camera | ZWO ASI1600MM-Cool, 4608×3504, mono, gain `[0, 600]`, offset `[0, 100]` — UniqueID `ZWO:ZWO-ASI1600MM-Cool:noserial-0`; seeded at connect at gain 600, offset 100 |
| ConformU | **4.5.0** build 53834.49ab847, against `http://127.0.0.1:11142/api/v1/camera/0` |

## Verdicts

| Suite | Result |
|---|---|
| `alpacaprotocol` | *"no errors, issues or information alerts"* — [log](alpacaprotocol.log) |
| `conformance` | *"no errors, warnings or issues found"*; every member within its target response time — [log](conformance.log), [results](conformance-results.json) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are all **0**. ConformU reads `ElectronsPerADU` once, before any gain write —
0.00496 e⁻/ADU, the figure at the seeded gain of 600 — and its gain and offset
checks read the seeded values, write each minimum and maximum, and are
refused `InvalidValue` one beyond each bound, all answered from the cache.

## A set during an exposure

Method: three 3 s frames per control. A at value *a*; B started at *a*, with
the value set to *b* 1.2 s into its exposure; C, the next frame, at *b*.
Statistics over every 97th pixel.

| | Offset 10 → 33 (gain 0), median / mean | Gain 0 → 300 (offset 100), median / mean |
|---|---|---|
| `main` — A | 224 / 224.7 | 1664 / 1664.2 |
| `main` — B (set in flight) | **592 / 592.2** | **1664 / 1664.1** |
| `main` — C | 592 / 592.0 | 4256 / 4408.9 |
| change — A | 160 / 157.9 | 1600 / 1597.7 |
| change — B (set in flight) | **160 / 157.8** | **1600 / 1597.6** |
| change — C | 528 / 525.8 | 2208 / 2297.1 |

On `main` an offset written while the camera integrates lands in that frame,
whole. The gain does not: this camera takes it at the start of the exposure,
so a gain set mid-exposure was already harmless to the frame on `main` —
only the offset was the defect here. On the change neither reaches the frame
in flight, and the next frame takes both. (The two runs differ in absolute
level because the camera was laid down again between them; each row compares
within its own run.)

## `ElectronsPerADU` follows the armed gain

| Step | `Gain` | `ElectronsPerADU` |
|---|---|---|
| gain 0, armed by an exposure | 0 | 4.96 |
| `Gain` set to 200, no exposure since | 200 | **4.96** |
| after one exposure armed 200 | 200 | 0.496 |
| gain 600 set back, armed by an exposure | 600 | 0.00496 |

The figure is the SDK's live read and describes the gain the camera holds:
after a set it catches up at the next exposure, 4.96 / 10^(200/200) = 0.496,
as ST2 now says.

## What arming costs

Ten 0.01 s frames at unchanged settings. `StartExposure` returns before the
capture arms, so the call itself does not move; the arm's two extra
`ASISetControlValue` writes sit inside `StartExposure` → `ImageReady`:

| | `StartExposure` call, median (max) | `StartExposure` → `ImageReady`, median |
|---|---|---|
| `main` | 0.5 ms (0.6) | 705 ms |
| change | 0.5 ms (0.6) | 708 ms |

## After the run

ConformU's last gain and offset writes were their maxima, 600 and 100 — the
values the camera was found at. It switched the cooler on during its run; a
reconnect afterwards read `CoolerOn = false`, so nothing was left running.

# zwo-camera — reconnect-during-exposure fix, three cameras, Fedora Linux (2026-08-23)

One `zwo-camera` instance serving **three physically attached ZWO bodies**, each
validated with both ConformU suites, plus a purpose-built reconnect harness for
the change under test: [#889](https://github.com/rusty-photon/rusty-photon/issues/889)
/ [PR #1056](https://github.com/rusty-photon/rusty-photon/pull/1056), which
replaced the handle-wide abort/stop signal with one cell per capture and gated
every post-start SDK call on the camera instance the capture began on.

The exposure state machine is what this PR rewired, so the run exists to put
abort, graceful stop, disconnect and reconnect on real hardware — the paths a
simulator can only approximate — and to check that a driver-internal change to
how captures are cancelled did not alter what a conformance validator sees.

## What was tested

| | |
|---|---|
| Commit | `2bc56edc` (branch `fix/zwo-camera-per-capture-stop-889`, PR #1056) |
| Platform | Fedora Linux 44 x86_64 (kernel 7.1.7-200) |
| Binary | `cargo run -p zwo-camera` — **default features**, i.e. the production non-`simulation` path `zwo-camera → zwo-rs → libzwo-sys → libASICamera2.so` |
| ZWO SDK | `/usr/local/lib/libASICamera2.so`, world-RW USB nodes |
| ConformU | 4.5.0 (Build 53834.49ab847) — matches the current upstream release, which is what `conformu.yml` installs |
| Service | one instance, port 11322, three devices at `camera/0`, `camera/1`, `camera/2` |

## Devices and verdicts

All six ConformU runs report `ErrorCount` / `IssueCount` /
`ConfigurationAlertCount` / `TimingIssuesCount` **all 0**, and every member
returned within its response target (87 / 82 / 74 timed members).

| Device | UniqueID | `alpacaprotocol` | `conformance` |
|---|---|---|---|
| ZWO ASI1600MM-Cool | `ZWO:ZWO-ASI1600MM-Cool:noserial-0` | clean | clean |
| ZWO ASI178MM | `ZWO:ZWO-ASI178MM:1915d5081b090900` | clean | clean |
| ZWO ASI120MC-S | `ZWO:ZWO-ASI120MC-S:1f19470620070900` | clean | clean |

## What each camera reported

| | ASI1600MM-Cool | ASI178MM | ASI120MC-S |
|---|---|---|---|
| Sensor type | Monochrome | Monochrome | RGGB (colour) |
| `BayerOffsetX/Y` | — | — | **1 / 0** (GRBG) |
| Sensor, reported | 4608 × 3504 | 3072 × 2064 | 1280 × 960 |
| `MaxBinX/Y` | 4 | 4 | 2 |
| `PixelSizeX/Y` | 3.8 µm | 2.4 µm | 3.75 µm |
| `MaxADU` | 65504 | 65528 | 65504 |
| `ElectronsPerADU` | 0.00496 | 0.00258 | 0.055 |
| Gain range | 0–600 | 0–510 | 0–100 |
| Offset range | 0–100 | 0–600 | 0–20 |
| Cooling (K1) | `true` | `false` | `false` |
| `CCDTemperature` | 0.0 °C at first read | 37 °C | 29 °C |

Every figure matches the [2026-08-07 three-camera record](../2026-08-07-zwo-camera-three-cameras-linux/README.md)
exactly, which is the point: the change is internal to how a capture is
cancelled, and nothing a client can observe about geometry, signal or cooling
moved. The ASI1600MM-Cool's `0.0 °C` first read is the documented SDK warm-up
artifact, not a caching defect (see the service design doc).

`CoolerOn` read `false` on every connect, and the TEC was confirmed off with
`CoolerPower = 0` after ConformU finished exercising it — tenet 3 holding, and
the run leaving the hardware as it found it.

## The exposure paths this PR rewired

Driven over Alpaca against each body, in addition to ConformU. **7/7 checks on
each of the three cameras.**

| Check | ASI1600MM-Cool | ASI178MM | ASI120MC-S |
|---|---|---|---|
| Plain 0.5 s / 2 s exposures return their own geometry | ✓ | ✓ | ✓ |
| **E7** `AbortExposure` discards the frame, device idle | ✓ 0.36 s | ✓ 0.10 s | ✓ 1.43 s |
| A new exposure is accepted right after an abort | ✓ 0.45 s | ✓ 0.10 s | ✓ 1.15 s |
| **E8** `StopExposure` preserves the partial frame | ✓ 0.32 s | ✓ 0.12 s | ✓ 1.08 s |
| **C3** disconnect cancels the in-flight exposure | ✓ | ✓ | ✓ |
| Normal exposure after all of the above | ✓ | ✓ | ✓ |

The timings are how long after the abort/stop the device reached `Idle` or
published the preserved frame — i.e. the per-capture cell draining a capture
out of a 20 s exposure, on the real SDK.

**E10, the reconnect contract**, was swept separately: start a full-frame
exposure, disconnect part-way, reconnect, then start a *differently sized*
exposure and require the second one to complete on time with its own geometry
and its own `LastExposureDuration`. The size difference is the tell — a frame
produced by the superseded capture carries the first exposure's dimensions.
Twelve disconnect points per camera, swept across integration **and** past the
exposure's end into the readout/download phase of a 12.7 MB frame: **12/12 on
each of the three bodies.**

## What this run could not show, and why that is expected

**The #889 race itself does not reproduce through the Alpaca API on a healthy
box, on either side of the fix.** `origin/main` (`232cc462`) was built in a
throwaway worktree and driven against the same ASI178MM by the same harness:
12/12 sweep trials clean, then 40/40 soak iterations clean, then 40/40 again
with the service pinned to a single core crowded by 64 spinners — starvation
that did bite (the second exposure slowed from 1.27 s to ~2.0 s) without ever
opening the window.

That is structural rather than luck. The window needs the reconnect **and** the
next `StartExposure` — ~300 ms of USB and SDK work — to complete before the
superseded capture's next 20 ms poll of the stop signal. Uniform CPU starvation
stretches both sides, so it does not flip the ratio; the window opens only when
that one capture thread is delayed past the reconnect, which is the
blocking-pool overshoot this repo has documented before (the ConformU/macOS
runner case in the service design doc). The fixed build was soaked under the
identical pinned-and-starved conditions for a like-for-like comparison: 40/40
clean, at matching wall clocks (1.98–2.09 s vs 2.02–2.15 s).

So the hardware evidence here is **no regression across every rewired path on
three bodies**, plus a severity calibration: the defect is real but needs a
badly starved capture thread, which is why the issue itself called for a
mock-backend hook. The deterministic proof lives in the unit tests, which were
verified to fail against the pre-fix shape:

- `a_reconnect_does_not_erase_a_superseded_captures_abort` — pre-fix outcomes
  `[Frame, Frame]`: the superseded capture ran to completion instead of seeing
  its abort.
- `a_superseded_capture_does_not_release_the_new_exposures_slot` — pre-fix
  `CameraState` read `Idle` while the second exposure was still running.
- `production_handle_capture_does_not_download_from_a_reopened_camera` —
  pre-fix the capture downloaded a frame off the camera reopened under it.

## Files

- `asi1600mm-cool-alpacaprotocol.log`, `asi1600mm-cool-conformance.log`,
  `asi1600mm-cool-conformance-results.json`
- `asi178mm-alpacaprotocol.log`, `asi178mm-conformance.log`,
  `asi178mm-conformance-results.json`
- `asi120mc-s-alpacaprotocol.log`, `asi120mc-s-conformance.log`,
  `asi120mc-s-conformance-results.json`

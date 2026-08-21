# qhy-camera on Linux — QHY178M + CFW, 2026-07-28

Recorded Linux ConformU run against a physical QHY178M **and the CFW filter
wheel attached to it**, covering both ASCOM devices the service registers from
one physical connection.

## What was tested

| | |
|---|---|
| Commit | [`7d9d93c9`](https://github.com/rusty-photon/rusty-photon/commit/7d9d93c9) — branch `fix/qhy-cfw-position-cache`, i.e. `origin/main` (`b2c73436`) plus the CFW position-cache fix this run required (see *Why this run needed a driver fix*) |
| Service | `qhy-camera`, **real-SDK** build (default features, no `QHYCCD_SKIP_NATIVE_LINK`) |
| Build | `cargo build --release -p qhy-camera`; rustc 1.96.0 |
| SDK | QHYCCD SDK **26.06.04** — `libqhyccd.so.26.6.4.16` in `/usr/local/lib`, sha256 `f51b92f9189fae7707e98ad334cf52d3c1493a6485f33394b39a18a3f4d5c738`; the version the repo pins (`docs/services/qhy-camera.md` § *Resolved facts*) |
| Platform | Fedora Linux 44, x86_64, kernel 7.1.4-204.fc44 — camera on the host's own USB 3 bus (no VM), `85-qhyccd.rules` udev rule, `usbfs_memory_mb` raised to 256 |
| Camera | QHY178M, 3056×2048 reported, mono, `MaxADU` 65535 — SDK id `QHY178M-222b16468c5966524` |
| FilterWheel | The CFW on that camera's port, **7 slots**, driven through the same physical `OpenQHYCCD` handle |
| ConformU | 4.3.0 build 49708, run on the same host against `http://127.0.0.1:11121/api/v1/camera/0` and `.../filterwheel/0` |

## Verdicts

Both devices, both suites, clean:

| Device | `alpacaprotocol` | `conformance` |
|---|---|---|
| Camera | 0 errors, 0 issues, 16 information messages — [log](alpacaprotocol-camera.log) | *"no errors, warnings or issues found"*, all members within their response-time targets — [log](conformance-camera.log), [results](conformance-camera-results.json) |
| FilterWheel | *"no errors, issues or information alerts"* — [log](alpacaprotocol-filterwheel.log) | *"no errors, warnings or issues found"*, all members within target — [log](conformance-filterwheel.log), [results](conformance-filterwheel-results.json) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are **0** in both results files (70 timed members on the camera, 33 on the
wheel).

The camera's 16 informational items are the protocol suite's casing probes
against `ImageArray`, `LastExposureDuration` and `LastExposureStartTime` before
any exposure has been taken — in-protocol ASCOM errors (`InvalidOperationException`
"no image available; ImageReady is false", `ValueNotSet`) returned with HTTP 200,
which is correct, so ConformU classifies them as informational rather than issues.

- **Camera `UniqueID`**: `QHY178M-222b16468c5966524`
- **FilterWheel `UniqueID`**: `CFW-QHY178M-222b16468c5966524` — the `CFW-`
  prefix that keeps it distinct from the camera's, which shares the same SDK id
  because both devices are one physical handle.

## Why this run needed a driver fix

The first attempt at the FilterWheel `conformance` suite passed ASCOM
validation but reported **2 timing flags**: `Position` at 0.256 s and
`DeviceState` (which aggregates it) at 0.261 s, both outside ConformU's 100 ms
FAST target. That fails this directory's all-four-counts-zero bar.

The cost was entirely the SDK: `GetQHYCCDCFWStatus` is a serial round-trip
through the camera and measures ~260 ms on this hardware, while `Names`,
`FocusOffsets` and `Connected` all answered in 0.00 s. The driver was calling it
on *every* `Position` read.

Since nothing moves the wheel except `set_position`, the settled slot is
knowable without asking — so the driver now caches it at connect and at the end
of each move, and reads the SDK only while a move is outstanding. INDI's
`indi-qhy` has always worked this way (`QueryFilter()` returns a cached member;
`GetQHYCCDCFWStatus` runs only while the move is `IPS_BUSY`). Measured on this
wheel after the change: settled reads 0.00 s, the `-1` moving sentinel
throughout a commanded move, and 0.00 s again once it lands.

## Windows

The same camera, wheel and commit pass equally cleanly on Windows 11 —
[2026-07-28 Windows record](../2026-07-28-qhy-camera-qhy178m-cfw-windows/README.md),
with identical `UniqueID`s and the identical set of 16 informational items.

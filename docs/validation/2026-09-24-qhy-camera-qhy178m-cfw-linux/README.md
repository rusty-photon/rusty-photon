# qhy-camera on Linux — QHY178M + CFW, 2026-09-24 (`Connecting` covers the handshake)

Recorded Linux ConformU run against the same physical QHY178M and 7-slot CFW as
the [2026-08-07 record](../2026-08-07-qhy-camera-qhy178m-cfw-linux/README.md),
taken after the fix that keeps `Connecting` true until **every** in-flight
`Connect` / `Disconnect` has finished.

It is the first QHY record since the C6 session work landed, and it exists
because the camera's `alpacaprotocol` suite had gone red in the interval: 1
error and 68 information messages, seventeen cache-backed members answering
`VALUE_NOT_SET`, `no valid binning modes` or `bin 1 is not a supported binning
mode` while the device reported `Connected == true`. The cause was not in this
service — see [C7](../../services/qhy-camera.md#enumeration--connection-lifecycle)
— and the same defect is what produced the FilterWheel log artefact the August
record documented. Both are gone here.

## What was tested

| | |
|---|---|
| Commit | [`72eefcef`](https://github.com/rusty-photon/rusty-photon/commit/72eefcef) |
| Service | `qhy-camera`, **real-SDK** build (default features, no `QHYCCD_SKIP_NATIVE_LINK`) |
| Build | `cargo build --release -p qhy-camera`; rustc 1.98.1 (48a229cea 2026-09-01) |
| SDK | QHYCCD SDK **26.06.04** — `/usr/local/lib/libqhyccd.so` → `libqhyccd.so.26.6.4.16`, sha256 `f51b92f9189fae7707e98ad334cf52d3c1493a6485f33394b39a18a3f4d5c738` (byte-identical to the August and July records, so the SDK is not a variable here) |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.4-200.fc44 |
| Camera | QHY178M, 3056×2048, mono, `MaxADU` 65535, `MaxBinX/Y` 2, readout modes `STANDARD MODE`, gain `[0, 51]`, offset `[0, 1023]` — SDK id `QHY178M-222b16468c5966524` |
| FilterWheel | The CFW on that camera's port, 7 slots, same physical `OpenQHYCCD` handle — `CFW-QHY178M-222b16468c5966524` |
| ConformU | **4.5.0** build 53834.49ab847, run against `http://127.0.0.1:11121/api/v1/camera/0` and `.../filterwheel/0` |

`UniqueID`s are unchanged from every earlier QHY record.

## Verdicts

Both devices, both suites, clean:

| Device | `alpacaprotocol` | `conformance` |
|---|---|---|
| Camera | 0 errors, 0 issues, 16 information messages — [log](alpacaprotocol-camera.log) | *"no errors, warnings or issues found"*, 70 timed members — [log](conformance-camera.log), [results](conformance-camera-results.json) |
| FilterWheel | *"no errors, issues or information alerts"* — [log](alpacaprotocol-filterwheel.log) | *"no errors, warnings or issues found"*, 33 timed members, all within target — [log](conformance-filterwheel.log), [results](conformance-filterwheel-results.json) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are **0** in both results files.

The camera's 16 informational items are the familiar set — the protocol suite's
four casing variants against each of `ImageArray`, `ImageArrayVariant`,
`LastExposureDuration` and `LastExposureStartTime` before any exposure exists,
answered with in-protocol ASCOM errors over HTTP 200. The July and August
records carried the same 16, and the recorded run reproduced them in four
consecutive `alpacaprotocol` runs with identical counts.

## What this pins about the connect window

- **A client that waits on `Connecting` never sees the handshake.** The
  recorded camera run is the direct evidence: the suite's four casing variants
  at `disconnect`, then four at `connect` ~18 ms later, and a property sweep
  that now finds the caches published. Against the previous behaviour the same
  78 ms sweep landed inside the handshake and read the caches C6 empties at the
  start of a connect.
- **The window itself is unchanged and still deliberate.** `Connected` is true
  from `OpenQHYCCD`, and the caches go live in one section at the end of the
  handshake (C6). What the fix restores is the only thing that ever separated a
  client from it. Filling the window with the previous session's values — the
  behaviour before C6, and the reason the August record was clean — would
  reinstate the bug C6 exists to prevent.
- **The August FilterWheel artefact was the same defect.** That record
  documented four `CameraNotOpen` / `NOT_CONNECTED` `ERROR` lines per wheel
  `alpacaprotocol` run, in a burst at the end, and read them as ConformU racing
  itself. They came from the same collapsed tracking: the connects finished
  against a handle a disconnect had already closed. The wheel run recorded here
  leaves **zero** such lines — the only two `ERROR` lines in the service log are
  the `INVALID_VALUE` refusals ConformU deliberately provokes on `Position`.

## Still open: overlapping connects race inside the SDK

The fix corrects what a client is *told*; it does not serialize what the
requests *do*. Four `Connect` requests arriving together still run four
handshakes concurrently on one shared `OpenQHYCCD` handle — four
`SetQHYCCDStreamMode` / `SetQHYCCDReadMode` / `InitQHYCCD` sequences at once —
and three of them lose the session race, answer `NOT_CONNECTED` and log
`request outlived the session it was made in`. The generation guard keeps their
*caches* out; nothing holds back their *SDK calls*.

ConformU no longer reaches that state, and it is not reachable by a client that
issues one connect at a time, so it did not block this record. It is still worth
closing — a per-device lock across `set_connected` is the shape — and it is
reproducible in a second with four concurrent `PUT /connect`.

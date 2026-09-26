# qhy-camera on Linux — QHY178M + CFW, 2026-09-26 (the connect window, covered and serialized)

Recorded Linux ConformU run against the same physical QHY178M and 7-slot CFW as
the [2026-08-07 record](../2026-08-07-qhy-camera-qhy178m-cfw-linux/README.md),
taken after the changes that close the connect window: `Connecting` now stays
true until **every** in-flight `Connect` / `Disconnect` has finished (C7);
`set_connected` runs one of them at a time, held **per physical connection**
rather than per ASCOM device, so the Camera and the CFW on one `OpenQHYCCD`
cannot handshake at once; and that order is taken by a task of the transition's
own, so a cancelled request cannot hand the connection on while the SDK calls it
was ordering are still running (C8).

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
| Commit | [`1dd801b8`](https://github.com/rusty-photon/rusty-photon/commit/1dd801b8) |
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
records carried the same 16, and the recorded run reproduced them in three
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

## A burst of connects is now one connect

`Connecting` corrects what a client is *told*; C8 corrects what the requests
*do*. Before it, four `Connect` requests arriving together each read a closed
handle, each concluded a connect was needed, and each sent a handshake —
`SetQHYCCDStreamMode` / `SetQHYCCDReadMode` / `InitQHYCCD` — down the one shared
`OpenQHYCCD`; three then lost the session race, answered `NOT_CONNECTED` and
logged `request outlived the session it was made in` for work the camera had
already done.

Driving four `PUT /disconnect` and then four `PUT /connect` at this build, the
service log carries **zero** `Error changing device connection state` and
**zero** `request outlived the session it was made in`, against three of each
before. The same burst is also visibly shorter — `Connecting` clears at 1.31 s
where it previously took 1.62 s, the difference being the three redundant
handshakes that no longer run.

Both ConformU service logs are clean of connection errors on both devices. The
only `ERROR` lines in either are the two `INVALID_VALUE` refusals the suite
deliberately provokes on the wheel's `Position`.

## And the two devices on one handle take turns

C8 is held on the shared connection, not on either device, so a Camera connect
and a FilterWheel connect issued together run one after the other. Firing both
at once on this rig, the service log has the wheel's handshake finishing —

```
21:09:31.135  filter wheel connected filter_wheel=CFW-QHY178M-… slots=7
```

— and every one of the camera's own SDK reads landing after it, in one block:

```
21:09:31.437  sensor geometry image_width_px=3056 image_height_px=2048 …
21:09:31.437  cached control range control="gain" min=0 max=51
21:09:31.437  cached control range control="offset" min=0 max=1023
21:09:31.437  camera connected camera=QHY178M-…
```

No interleaving, and zero connection errors. The waiting is visible from the
client side too: the camera answered `Connecting = true, Connected = false` for
~1 s while the wheel held the connection, then completed.

A lock per device instead lets both handshakes run at once — the camera asking
`SetQHYCCDStreamMode` / `InitQHYCCD` while the wheel asks `CfwSlotsNum`, two
threads in the SDK on one handle. That is what the unit test
`a_wheel_connect_waits_for_a_camera_connect_on_the_same_handle` pins; this is the
same thing on the hardware.

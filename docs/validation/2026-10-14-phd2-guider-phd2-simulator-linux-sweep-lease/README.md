# phd2-guider camera facade — the sweep lease against PHD2 2.6.14 Simulator, Linux (device claims C7b)

The facade's sweep lease
([phd2-guider.md § The sweep lease](../../services/phd2-guider.md#the-sweep-lease))
and its frames sent as `UInt16`, run against a live PHD2 on the field rig.
ConformU 4.5.0 ran its `alpacaprotocol` and `conformance` suites against
the facade, and both were **clean**: `conformance-results.json`'s
`ErrorCount`, `IssueCount`, `ConfigurationAlertCount` and
`TimingIssuesCount` are all 0. A probe then drove the lease through every
rule it could reach with a real PHD2, and passed each one.

This is C7b's hardware check, which the device-claims plan asks of each C7
slice. **The camera behind PHD2 was PHD2's own simulator**, as in C6's
[first record](../2026-10-13-phd2-guider-phd2-simulator-linux/README.md).
The lease depends on PHD2's state and the service's own records, not on
the camera, and the simulator left `qhy-camera`, its udev rule and the
guide camera untouched. The lease held through a sweep with the
QHY5III715C is C7d's hardware check.

## What was tested

| | |
|---|---|
| Commit | [`83210804`](https://github.com/rusty-photon/rusty-photon/commit/83210804) (branch `feature/device-claims-c7b`, PR #1517). The head this record ships with adds only a `mock_phd2` change on top, which is a test binary |
| Host | the field rig (Raspberry Pi 5, Raspberry Pi OS, Debian 13, aarch64) |
| Build | `cargo run --release -p phd2-guider --bin phd2-guider -- --config <file> serve` on the rig (cargo 1.99.0), no features; no native SDK is involved in the facade. `RUST_LOG=info,phd2_guider=debug`, so `service.log` shows each lease's grant, renewal, release and end |
| PHD2 | 2.6.14, source-built on the rig, headless under TigerVNC as the operator, equipment profile "Simulator" (PHD2's built-in camera and mount simulator), camera 752 × 580. Started for the run and shut down after it (`set_connected false`, then `shutdown`), as it was found |
| Service account | The operator's. PHD2 and the service both ran as the operator, with `camera.image_dir` in the operator's home. The service account's sandbox and the shared-directory recipe were tested in C6's simulator record and are not repeated here |
| Config | REST on `127.0.0.1:11230` and the facade on `127.0.0.1:11228`, next to the packaged guider on 11130, which ran throughout and was not touched. Pixel size 4.6 µm (the simulator's). Defaults otherwise (`capture_grace` 10 s) |
| Device identity | `UniqueID` `4a58a539-85cd-4637-8dab-9755c378e879`, minted into the config file at the first enabled start (`service.log` line 5) |
| ConformU | 4.5.0 (Build 53834), the latest release on the day. It ran on the dev box (Fedora Linux 44 x86_64) through an SSH tunnel to the rig's loopback port. The settings file was `{"SettingsCompatibilityVersion": 1}` |

## Results

| Suite | Result |
|---|---|
| `alpacaprotocol` | 0 errors, 0 issues, 20 information messages |
| `conformance` | `ErrorCount` 0, `IssueCount` 0, `ConfigurationAlertCount` 0, `TimingIssuesCount` 0. `SupportedActions` lists the three lease Actions, then the config Actions; ConformU does not call an Action ("Conform cannot test the Action method"). One full-frame exposure, read as a 752 × 580 image in 82 ms (C6's simulator record: 184 ms, as `Int32`) |

ConformU's "Successfully read 32 bit integer array" names the image's
element type, which ImageBytes always reports as `Int32`
(`ImageElementType` 2). The type on the wire is its
`TransmissionElementType`, which the probe read as 8, `UInt16` (below).

## The lease against PHD2

`probe/leaseprobe.py` ran on the rig. It drove the facade over loopback,
called the REST API, and read PHD2 as a second event-server client.
Unlike C6's probes it asserts every outcome: it stops at the first one
that does not hold and exits 1. It ran to its `done` line
(`probe/probe.jsonl`), so every check below held:

| Check | Outcome |
|---|---|
| An acquire on a disconnected camera | `NotConnected` (1031) |
| `SupportedActions` | the three lease Actions first |
| An acquire under token `a…a`, `ttl_s` 30 | `{"token": "a…a", "ttl_s": 30}` |
| `guiding/start`, `guiding/pause`, `dither`, `calibration/clear` while it is held | 409 `camera_busy`, naming the sweep lease; PHD2 still `Stopped` afterwards, so none reached it |
| A 1 s exposure under the lease | `StartExposure` answered in 45 ms; `ImageReady` 1.061 s after that |
| `ImageArray` as ImageBytes | header `[1, 0, 30, 30, 44, 2, 8, 2, 752, 580, 0]`: `TransmissionElementType` 8, `UInt16`; 872 364 bytes, the 44-byte header and 752 × 580 × 2 bytes of pixels, read over loopback in 4.8 ms |
| A renewal | `ttl_s` 30 again |
| A second acquire while one is held | `InvalidOperation` (1035), "already held" |
| Release, then release again | `{"released": true}`, then `{"released": false}` |
| An acquire under the released token | 1035, "used before" |
| An acquire while PHD2 loops (another client sent `loop`) | 1035, "PHD2 is Looping" |
| `guiding/stop` under a lease, then a renewal | 200; the renewal 1035, "ended by a guiding stop" |
| `Connected = false` under a lease, then a renewal | the renewal 1035, "ended when the camera was disconnected" |
| A 10 s lease left alone for 11 s, then a renewal | 1035, "expired at …: no renewal came within its TTL"; `calibration/clear` then 200 |
| An acquire sent as soon as a 3 s `StartExposure` answered | 1035, naming when the frame ends "at the latest" |

`service.log` shows the same transitions from the service's side: each
`sweep lease granted`, `renewed`, `release`, and `the sweep lease ended`
with its reason (`Released`, `Stopped`, `Disconnected`), and `the sweep
lease expired` at the TTL. `image_dir` was empty after the run.

## PHD2's simulator has no frame size until its first frame

With the Simulator profile's equipment connected and no frame taken,
PHD2 2.6.14 answered `get_camera_frame_size` with `[0, 0]`. After one
100 ms `capture_single_frame`, sent to PHD2 directly before the service
started, it answered `[752, 580]`. The facade's connect reads the frame
size and does not refuse `[0, 0]`, so a facade connected before PHD2's
first frame would serve a 0 × 0 sensor. That is the facade's connect from
C6, not the lease, and is filed as
[#1518](https://github.com/rusty-photon/rusty-photon/issues/1518).

## Privacy scrub

`service.log` and `probe/probe.jsonl` had the operator's home directory
replaced with `<home>`. In `service.log` the rig's hostname, in each PHD2
event's `Host` field, was replaced with `<rig-host>`, and its terminal
colour codes were removed. The ConformU files (`alpacaprotocol.log`,
`conformance.log`, `conformance-results.json`) are unmodified.

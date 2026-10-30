# phd2-guider camera facade — a connect before PHD2's first frame, PHD2 2.6.14 Simulator, Linux

The facade's connect when PHD2 does not know its camera's frame size yet
([phd2-guider.md § A frame size PHD2 does not know yet](../../services/phd2-guider.md#a-frame-size-phd2-does-not-know-yet)),
run against a live PHD2 on the field rig. PHD2 2.6.14's Simulator camera
reports a 0 × 0 frame until it has taken one. The facade's connect then
takes one 1 ms frame and serves the size PHD2 reports after it. A probe
that asserts each outcome connected the facade twice, once before PHD2's
first frame and once after, and ran to its `done` line.

[#1518](https://github.com/rusty-photon/rusty-photon/issues/1518) was
found in the [2026-10-14 record](../2026-10-14-phd2-guider-phd2-simulator-linux-sweep-lease/README.md#phd2s-simulator-has-no-frame-size-until-its-first-frame):
a facade connected before PHD2's first frame served a 0 × 0 sensor.
[#1572](https://github.com/rusty-photon/rusty-photon/pull/1572) fixed it.
This record is that fix checked against a live PHD2.

**This record carries no ConformU run.** It proves one connect path. The
fix changed only what `Connected = true` does while PHD2 reports 0 × 0.
The facade's Alpaca surface ran clean through ConformU in the
[2026-10-13](../2026-10-13-phd2-guider-phd2-simulator-linux/README.md)
and 2026-10-14 records.

## What was tested

| | |
|---|---|
| Commit | [`256543e6`](https://github.com/rusty-photon/rusty-photon/commit/256543e6) (`main`), which carries #1572 |
| Host | the field rig (Raspberry Pi 5, Raspberry Pi OS, Debian 13, aarch64) |
| When | 2026-10-30, 05:57 UTC |
| Build | `cargo run --release -p phd2-guider --bin phd2-guider -- --config <file> serve` on the rig (cargo 1.99.0), no features; no native SDK is involved in the facade. `RUST_LOG=info,phd2_guider=debug`, so `service.log` shows each request the facade sent PHD2 |
| PHD2 | 2.6.14, built on the rig from its `v2.6.14` tag, headless under TigerVNC as the operator, equipment profile "Simulator" (PHD2's built-in camera and mount simulator), camera 752 × 580. PHD2 was not running beforehand. It was started for the run, and its equipment was connected from a second event-server client with `set_connected true`, with no frame taken. Afterwards it was disconnected with `set_connected false` and shut down with `shutdown`, as it was found |
| Service account | The operator's. PHD2 and the service both ran as the operator, with `camera.image_dir` in the operator's home, as in the 2026-10-14 record |
| Config | REST on `127.0.0.1:11230` and the facade on `127.0.0.1:11228`. Pixel size 4.6 µm (the simulator's), defaults otherwise (`capture_grace` 10 s). The packaged guider ran on 11130 throughout, untouched. Its facade is off on the rig |
| Device identity | `UniqueID` `021b2375-e0f2-45e3-95e3-b620c91eb5c4`, minted into the config file at the first enabled start (`service.log` line 5) |

## The connect against PHD2

`probe/frameprobe.py` ran on the rig. It drove the facade over loopback
and read PHD2 as a second event-server client. It asserts every outcome:
it stops at the first one that does not hold and exits 1. It ran to its
`done` line (`probe/probe.jsonl`), so every check below held.

| Check | Outcome |
|---|---|
| Before the run | PHD2 reported 2.6.14 and its "Simulator" profile, and its Simulator camera was connected. PHD2 was `Stopped`. `image_dir` was empty, and the facade read `Connected` false |
| PHD2's frame size before the first connect | `[0, 0]` |
| The first `Connected = true` | succeeded in 85 ms |
| PHD2's frames during it | exactly one `SingleFrameComplete`, `Success: true`, for a `phd2-guider-…` file in `image_dir`. The file was gone when the connect answered |
| PHD2's frame size after it | `[752, 580]` |
| What the facade served | `CameraXSize` 752, `CameraYSize` 580, `SensorName` "Simulator" |
| A second `Connected = true`, after `Connected = false` | succeeded in 1 ms with no frame, serving 752 × 580 |
| `image_dir` after | empty: no frame file and no restart marker |

The probe counts frames without waiting. PHD2 sends an event to every
client before it reads its next request, and the facade answers a connect
that takes a frame only after that frame's `SingleFrameComplete`. When
PHD2 answers the probe's next request, any `SingleFrameComplete` that the
connect caused has therefore already reached the probe.

`service.log` shows the first connect from the facade's side, in lines
19–41:

| Line | |
|---|---|
| 21 | `get_camera_frame_size` answered `[0,0]` |
| 25 | `PHD2 does not know its camera's frame size yet; taking a 1 ms frame` |
| 27 | PHD2 reported `Stopped` |
| 28 | the capture slot was claimed |
| 30 | `capture_single_frame` went out with only `exposure` 1, `path` and `save`: no binning, gain or subframe |
| 34 | `SingleFrameComplete`, 69 ms after the request |
| 36–37 | the slot was released and the file deleted |
| 40–41 | `get_camera_frame_size` answered `[752,580]`, and the camera connected at that size |

The second connect, lines 42–48, read `[752,580]` at once and sent PHD2
no capture.

The same steps had run once before, seven minutes earlier, with a probe
that printed its outcomes instead of asserting them. Every outcome was
the same, and that first connect took 94 ms.

## PHD2 sends a ConfigurationChange for each single frame

1.2 ms after the facade's `capture_single_frame`, PHD2 sent a
`ConfigurationChange` event (`service.log` line 31). The facade did not
cause it: the request names no setting. PHD2 2.6.14's
`capture_single_frame` handler (`src/event_server.cpp:1382`) fills a
missing `binning` or `gain` with the camera's current value. It then
passes both to `SingleExposure::Activate` (`src/myframe_events.cpp:415`),
which sets them through `SetBinning` (line 430) and `SetCameraGain`
(line 435). Each of those writes its value into the profile
(`src/camera.cpp:734` and `:714`), and each profile write raises a
`ConfigurationChange`, debounced to one event (`src/event_server.cpp:3026`).
The profile's camera binning (1) and gain (95) read the same after the
run as before it. The frame changes none of PHD2's settings. It makes
PHD2 write two of them back unchanged.

## Privacy scrub

In `service.log` and `probe/probe.jsonl`, the operator's home directory
was replaced with `<home>`. In `service.log`, the rig's hostname in each
PHD2 event's `Host` field was replaced with `<rig-host>`, and the
terminal colour codes were removed. The probe leaves each event's `Host`
field out of `probe.jsonl`.

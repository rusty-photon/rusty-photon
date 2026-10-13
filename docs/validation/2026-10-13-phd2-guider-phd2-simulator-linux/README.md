# phd2-guider camera facade — PHD2 2.6.14 Simulator, Linux (device claims C6)

`phd2-guider`'s opt-in camera facade
([phd2-guider.md § Camera facade](../../services/phd2-guider.md#camera-facade-camera))
against a live PHD2. ConformU 4.5.0 ran its `alpacaprotocol` and
`conformance` suites, and both were **clean**: every count in
`conformance-results.json` is 0.

The same session made the measurement the device-claims plan's C6 was
waiting on: how PHD2 2.6.14 completes a single frame (plan D9, "Settled in
C6"). It also ran the `image_dir` sharing recipe from
[packaging.md](../../packaging.md#phd2-guider-the-camera-facade) as the
acceptance test the plan asks for: a frame PHD2 wrote, read and deleted by
the service account.

**The camera behind PHD2 was PHD2's own simulator, not a guide camera.**
PHD2's handling of a single frame, the event, the file and every refusal
below are PHD2's own code and do not depend on the camera. Two things do:
whether `stop_capture` cuts an exposure short, which PHD2 leaves to each
camera driver (D9), and the frame's size and pixels. A run with the rig's
QHY5III715C behind PHD2 is still owed. It needs `qhy-camera` given a
`usb_devices` list that leaves that camera off, then a PHD2 profile that
uses it.

## What was tested

| | |
|---|---|
| Commit | [`5238ccd1`](https://github.com/rusty-photon/rusty-photon/commit/5238ccd1) (branch `feature/device-claims-c6-phd2-camera`) |
| Host | the field rig (Raspberry Pi 5, Raspberry Pi OS, Debian 13, aarch64) |
| Build | `cargo build --release -p phd2-guider` on the rig, no features; no native SDK is involved |
| PHD2 | 2.6.14, source-built on the rig, headless under TigerVNC as the operator, equipment profile "Simulator" (PHD2's built-in camera and mount simulator), camera 752 × 580 |
| Service account | `rusty-photon`, the packaged services' user. The branch binary ran as a transient systemd unit (`systemd-run --uid=rusty-photon`) with the packaged unit's sandbox: `ProtectSystem=strict`, `ReadWritePaths=/var/lib/rusty-photon`, `ProtectHome=yes`, `PrivateTmp=yes`, `UMask=0027`, `NoNewPrivileges=yes` |
| Config | REST on `127.0.0.1:11230` and the facade on `127.0.0.1:11228`, next to the packaged guider on 11130. `camera.image_dir` = `/var/lib/rusty-photon/phd2-images`, pixel size 4.6 µm (the simulator's) |
| Device identity | `UniqueID` `6fae39a0-aede-4dfb-9441-d35912b94378`, minted into the config file at the first enabled start (`service.log` line 4) |
| ConformU | 4.5.0 (Build 53834), the latest release on the day. It ran on the dev box (Fedora Linux 44 x86_64) through an SSH tunnel to the rig's loopback port. The settings file was `{"SettingsCompatibilityVersion": 1}` |

## Results

| Suite | Result |
|---|---|
| `alpacaprotocol` | 0 errors, 0 issues, 20 information messages |
| `conformance` | `ErrorCount` 0, `IssueCount` 0, `ConfigurationAlertCount` 0, `TimingIssuesCount` 0. One full-frame 2 s exposure, read as a 752 × 580 image in 184 ms; every negative and out-of-sensor `StartExposure` rejected |

## Acceptance: a frame PHD2 wrote, read and deleted by the service account

1. **The recipe, by hand.** The four commands of packaging.md's step 1
   were run as given. PHD2, running as the operator, then saved a 0.5 s
   frame to `/var/lib/rusty-photon/phd2-images/accept-1.fits`. The file
   was the operator's (`rw-rw----`), and the default ACL gave
   `group:rusty-photon` `rw-`. `sudo -u rusty-photon` read all 875 520
   bytes and deleted it.
2. **Through the facade, under the unit's sandbox.** Startup's
   `image_dir` check (`.phd2-guider-probe` created and removed) passed.
   packaging.md's step 3 then ran through the tunnel: `Connected`,
   `CameraXSize` 752, `SensorName` "Simulator", and a 1 s exposure gave
   `ImageReady` true. `service.log` lines 31–41 show the slot claimed,
   PHD2 accepting the frame, and the slot released on PHD2's event
   1.03 s after that acceptance. They also show the facade's frame file
   deleted before the image was published. `image_dir` was empty
   afterwards.

## The single-frame measurement (plan D9)

`phd2-probe/phd2probe.py` connected to PHD2's event server as a second
client and logged every event and reply with a time from the probe's
start. Its outputs are in `phd2-probe/`:

- `baseline.jsonl`: the version, the profile, and the exposure list,
  which is not what `capture_single_frame` is limited to.
- `path.jsonl`: frames saved to a named path, saved to PHD2's own
  directory, and not saved; the state during each frame; the 1, 10 and
  100 ms frames.
- `interfere.jsonl`: a second capture, `loop`, `stop_capture` and a full
  pause, each sent during a frame. The pause left PHD2 refusing every
  capture. That is why this file's `bad_params`, `bad_dir`, `disconnected`
  and `looping_refusal` sections show only that refusal, and why they were
  run again after a PHD2 restart.
- `errors.jsonl`: the parameter refusals, a directory that does not
  exist, the camera disconnected, and `guide` during a frame.

PHD2's own debug log for the paused frame (not committed). The lines
quoted, with its thread and delta columns dropped:

```text
14:36:09.189 SetPaused type=2 isPaused=0 exposurePending=1
14:36:11.702 guider is paused, ignoring frame, not scheduling exposure
14:36:17.519 SetPaused type=0 isPaused=1 exposurePending=0
14:36:25.894 StopCapturing CaptureActive=1 continueCapturing=0 exposurePending=0
```

The raw reply with the camera disconnected, read off the socket:

```text
{"jsonrpc":"2.0","error":{"code":1,"message":"cannot capture single frame when camera is not connected"},"error":{"code":2,"message":"failed to start exposure"},"id":3}
```

## Service log

`service.log` is the facade's journal for the whole session, at
`RUST_LOG=debug`. Of its 283 `WARN`/`ERROR` lines, every one has an
`ascom_alpaca::server` target, and none comes from the facade's own
modules:

| Count | Kind |
|---|---|
| 148 | `WARN` Alpaca method is not implemented: the optional members the facade does not implement (cooler, gain, offset, Bayer offsets, fast readout, pulse guiding, sub-exposure, `StopExposure`) |
| 112 | `WARN` Unused parameters: ConformU's protocol probes |
| 10 | `INVALID_OPERATION` no exposure has started yet: `LastExposureDuration`/`LastExposureStartTime` read before any exposure |
| 4 | `INVALID_VALUE` dark frames are not supported |
| 4 | `INVALID_VALUE` a bin other than 1 |
| 4 | `INVALID_VALUE` a subframe that does not fit the sensor |
| 1 | `INVALID_VALUE` a negative `Duration` |

## Privacy scrub

`service.log` and the files in `phd2-probe/` had the rig's hostname
replaced with `<rig-host>`, the operator's account with `<operator>`, and
the operator's home directory with `<home>`. In `service.log` that is
every line's hostname field. In the probe outputs it is each event's
`Host` field and the paths PHD2 chose under its own directory. The
ConformU files (`alpacaprotocol.log`, `conformance.log`,
`conformance-results.json`) are unmodified.

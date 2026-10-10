# qhy-camera on Windows: QHY600M + CFW on rig2, 2026-10-10 (a connection's first wheel move)

This is a recorded Windows ConformU run against rig2's QHY600M and the CFW on its port. It
covers PR #1452's head, which carries
[FW7](../../services/qhy-camera.md#filterwheel-when-a-cfw-is-detected) as well as the
PR's earlier changes. It is the Windows companion to the
[Linux record of the same day](../2026-10-10-qhy-camera-qhy178m-cfw-linux-first-move/README.md),
whose measurements show that the Windows SDK reports the CFW's `N` while the wheel moves.

So the case FW7 exists for cannot arise here: a first move to slot 0 that reads as
arrived at once. What this run shows is the change running under Windows:
- ConformU clean on both devices, with its first wheel move going through the prime;
- `Position` reading "moving" until the wheel arrives;
- what the prime costs on this camera;
- a wheel that has reported itself moving not being primed again.

Times are the rig's, CDT (UTC−5). The run took place between 17:56 and 18:08.

## What was tested

| | |
|---|---|
| Commit | [`70d0c9c7`](https://github.com/rusty-photon/rusty-photon/commit/70d0c9c7), branch `fix/qhy-camera-connect-owns-device`, PR #1452: the FW7 change [`75837229`](https://github.com/rusty-photon/rusty-photon/commit/75837229) with `main` merged in |
| Build | `cargo build --locked --release -p qhy-camera`, built in the dev box's Windows 11 VM with rustc 1.99.0 (b940084d7 2026-09-28) against rig2's own `qhyccd.lib`, from its QHY All-in-One pack (24.01.09). `qhy-camera.exe` sha256 `2c398411af7df313bf70730400d7cca0d24f6a2a5f6a8799634a44729f7ef4e5`, checked on the rig |
| How it ran | The installed service (nightly `g689f851`) was stopped, and the branch exe ran in console mode with a plain-HTTP configuration bound to `127.0.0.1:11141`, `"devices": {}` and `--log-level debug`. It enumerated the QHY600M and its CFW as camera 0 and filter wheel 0; the rig's other QHY camera was not on USB. Each step below ran in a fresh process. The probes were driven from the dev box through an ssh tunnel, with a round trip of 0.12–0.14 s; ConformU ran on the rig. The service was started again afterwards |
| SDK | `qhyccd.dll` **24.1.9.12** from the QHY All-in-One (`C:\Program Files\QHYCCD\AllInOne\sdk\x64`, the copy the installed service loads), sha256 `f99916434e3734372a051cc791f4629d7d6801ee7077d48e3315af1f848194ea`. The driver's preflight resolved it there, and each process's loaded module was checked |
| Platform | Windows 11 Pro 25H2 x64 (build 26200.9457), Intel Core i3-1220P |
| Camera | QHY600M, firmware 2023-06-14 (`QHY600U3G20-20230614`), 9576×6384 in mode 0, ten readout modes listed by this SDK. SDK id `QHY600M-3b1ea54688eb9f9d7` |
| FilterWheel | The CFW on that camera's port, 7 slots, on the same `OpenQHYCCD` handle: `CFW-QHY600M-3b1ea54688eb9f9d7` |
| ConformU | **4.5.0** build 53834.49ab847, the latest release, run on the rig against `http://127.0.0.1:11141/api/v1/camera/0` and `.../filterwheel/0`, with a settings file holding only `{"SettingsCompatibilityVersion": 1}` |

## Verdicts

Both devices, both suites, clean:

| Device | `alpacaprotocol` | `conformance` |
|---|---|---|
| Camera | 0 errors, 0 issues, 16 information messages: [log](alpacaprotocol-camera.log) | *"no errors, warnings or issues found"*; every member within its target response time: [log](conformance-camera.log), [results](conformance-camera-results.json) |
| FilterWheel | *"no errors, issues or information alerts"*: [log](alpacaprotocol-filterwheel.log) | *"no errors, warnings or issues found"*; every member within its target response time: [log](conformance-filterwheel.log), [results](conformance-filterwheel-results.json) |

`ErrorCount`, `IssueCount`, `ConfigurationAlertCount` and `TimingIssuesCount` are **0** in
both results files. The camera's 16 information messages are the set every QHY record
carries: four casing variants each against `ImageArray`, `ImageArrayVariant`,
`LastExposureDuration` and `LastExposureStartTime`, before any exposure exists.

ConformU's filter-wheel suite ran in a fresh process with the wheel at slot 4. Its first
move, to slot 0, was the first move of that process, so it went through the prime:
- ConformU timed the `Position` write at **0.814 s**, inside its 1 s target for an
  asynchronous initiator, and the wheel reached slot 0 in 4.9 s;
- its other 19 moves were written in 0.010–0.054 s;
- one slot to the next took 1.8 s.

## A connection's first move through the service

Each run used a fresh process, with the wheel standing at slot 4:
1. connect the filter wheel;
2. after 1 s, write `Position = 0` and read `Position` for 8 s;
3. write `Position = 4`;
4. disconnect and reconnect;
5. write `Position = 2`, then `Position = 4`.

A wait of 1 s came before each later move. Times are from the client, through the tunnel,
counted from the start of each write.

| | Run 1 | Run 2 |
|---|---|---|
| First move, 4 → 0: the write returns | 0.924 s | 0.907 s |
| `Position` after it | −1 from the first read (1.16 s) to 4.62 s, then 0 from 5.24 s | −1 from 1.16 s to 4.60 s, then 0 from 5.24 s |
| Second move, 0 → 4: write / arrived | 0.180 s / 5.42 s | 0.161 s / 5.92 s |
| After a reconnect, 4 → 2: write / arrived | 0.164 s / 3.18 s | 0.153 s / 3.18 s |
| 2 → 4: write / arrived | 0.171 s / 3.18 s | 0.156 s / 3.18 s |

The driver's debug log times the prime itself. In both runs, the line saying it was
sending the wheel to slot 4, the slot it stood on, came 1–10 ms after the write arrived.
The driver logged its 250 ms rest before the move to slot 0 another 0.50 s later. Those
0.50 s held the prime's command and the status read that named slot 4. So the write takes
about 0.80 s at the driver, and the tunnel adds the rest. ConformU's first move logged
the same 0.50 s.

That read is the slow part. In the `cfw_probe` runs of the
[Linux record (§2)](../2026-10-10-qhy-camera-qhy178m-cfw-linux-first-move/README.md):
- the command to the slot the QHY600M stood on took 24–37 ms;
- the status read after it took 455–476 ms;
- each read after that took ≈256 ms.

A third fresh process, used beforehand to take the wheel off slot 0, primed its first move
(0 → 4) in the same way: 0.50 s from the prime's command to the rest, and 0.918 s for the
write through the tunnel.

After the reconnect the driver logged no prime: the move to slot 2 went straight out.
That is FW7's rule for a wheel that has already reported itself moving, which the driver
learns only from a status read that decodes as `CfwStatus::Moving`. The second move's
write, and every move after the first, took the tunnel's round trip plus about
0.03–0.06 s.

## After the run

- **Power:** the camera's 12 V output was off when the session began. It was switched on
  through the rig's power hub at 17:56 for the run and off again at 18:08.
- **Wheel position after the power cycle:** the probe runs of the Linux record had left
  the wheel at slot 5 at 16:07. The first process here found it at slot 0. This is
  consistent with the power-up homing measured on the dev box's CFW (C5). It is not a
  controlled measurement: the installed service ran in between.
- **Camera settings:** a connect before ConformU read gain 30, offset 30, mode 0, bin 1,
  the full frame and the cooler off. A connect after ConformU read the same, so nothing
  needed restoring. The sensor read 15.8 °C before and 21.8 °C after.
- **Wheel:** ConformU left it at slot 0.
- **Service:** the installed service was started again at 18:07 and found its camera and
  wheel (`cameras=1 filter_wheels=1`).

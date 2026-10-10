# qhy-camera on Linux: QHY178M + CFW, 2026-10-10 (a connection's first wheel move)

This is a recorded Linux ConformU run against the same physical QHY178M + CFW as the
[2026-10-10 wheel record](../2026-10-10-qhy-camera-qhy178m-cfw-linux-wheel/README.md).
It covers the change in [FW7](../../services/qhy-camera.md#filterwheel-when-a-cfw-is-detected)
(PR #1452), which has two parts:
- `qhyccd-rs` decodes a CFW's `'N'` as the wheel moving, and `Position` reports it so;
- a connection's first move first sends the wheel to the slot it stands on, unless
  the wheel has already reported itself moving.

Beside the ConformU run are the measurements behind FW7 and two additions to FW5 and
C5. They come from three places:
- this QHY178M under Linux;
- the same QHY178M passed through to a Windows 11 VM on the dev box;
- the Windows field rig's QHY600M (rig2), each Windows setup with two SDK builds.

Nothing here is optical. The dev box's wheel is not in the camera's light path, and on
rig2 no frame was taken. Everything is what the CFW reports and how long each status
read takes: ≈255 ms with the wheel at rest, ≈100 ms while it travels.

## What was tested

| | |
|---|---|
| Commit | [`75837229`](https://github.com/rusty-photon/rusty-photon/commit/75837229) (branch `fix/qhy-camera-connect-owns-device`, PR #1452), on `main` at `7fdbf4e6` |
| Service | `qhy-camera`, **real-SDK** build (default features), `cargo run -p qhy-camera -- --log-level debug` (dev profile); rustc 1.99.0 (b940084d7 2026-09-28) |
| Control | The PR's previous head, [`317b2ccf`](https://github.com/rusty-photon/rusty-photon/commit/317b2ccf), exported with `git archive` and run the same way |
| Probe | `qhyccd-rs`'s [`cfw_probe`](../../../crates/qhyccd-rs/examples/cfw_probe.rs) at `317b2ccf`; the Linux runs at 13:19 PDT ran the same modes from the tree just before it, which lacked only the `CFW_PROBE_CAMERA` camera choice. On Linux: `cargo build -p qhyccd-rs --example cfw_probe`, run with the service stopped. For Windows: built in the VM against rig2's own `qhyccd.lib` (the All-in-One pack's, 24.01.09), exe sha256 `459c0e7af8262ed0c99edb99ed80a2048621df47ed6e65b0a9225e557912ccc5`, the same exe on both Windows hosts |
| SDK, Linux | QHYCCD SDK **26.06.04**: `libqhyccd.so.26.6.4.16`, sha256 `f51b92f9189fae7707e98ad334cf52d3c1493a6485f33394b39a18a3f4d5c738` (byte-identical to every earlier QHY record) |
| SDK, Windows | `qhyccd.dll` placed beside the probe exe, and each process's loaded module checked: **26.06.04** (file version 26.6.4.16, sha256 `c7cea0039c3719388dcbb38f02524d4bdc6aaa827495056a2ec3b5bb24551d5f`, the file of the [2026-10-06 Windows record](../2026-10-06-qhy-camera-qhy178m-cfw-windows-departure/README.md)); **26.07.28** (26.7.28.15, `3bba995dc586cdd6b1bd4de4387f845823ccf0da589a5030262cfdbf9ccba2b3`); **24.01.09** (24.1.9.12, `f99916434e3734372a051cc791f4629d7d6801ee7077d48e3315af1f848194ea`, rig2's All-in-One pack, from its install directory) |
| Platforms | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.8-200.fc44. The dev box's Windows 11 Pro KVM guest (build 26200), the camera passed through as a USB `hostdev`, as in the 2026-10-06 record. Rig2: Windows 11 Pro (build 26200) on an Intel NUC |
| Cameras | QHY178M-Cool, SDK id `QHY178M-222b16468c5966524`, with its 7-slot CFW3, powered through the PPBA's Quad 12V output as in the earlier records. Rig2's QHY600M, SDK id `QHY600M-3b1ea54688eb9f9d7`, with its 7-slot CFW |
| ConformU | **4.5.0** build 53834.49ab847, against `http://127.0.0.1:11121/api/v1/{camera,filterwheel}/0` |

The Quad 12V output was switched on before anything below ran, and the service
enumerated both devices on each start (`cameras=1 filter_wheels=1`). The SDK ran on its
defaults: no `qhyccd.ini` in any working directory.

## Verdicts

| Device | Suite | Result |
|---|---|---|
| Camera | `alpacaprotocol` | 0 errors, 0 issues, 16 information messages ([log](alpacaprotocol-camera.log)) |
| Camera | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-camera.log), [results](conformance-camera-results.json)) |
| FilterWheel | `alpacaprotocol` | no errors, issues or information alerts ([log](alpacaprotocol-filterwheel.log)) |
| FilterWheel | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-filterwheel.log), [results](conformance-filterwheel-results.json)) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount` are all
**0** in both results files. The 16 informational items are the set every QHY record
carries. ConformU's first FilterWheel move, on a service just started, went from slot 4
to slot 0: the case FW7 is about. It reached slot 0 in 4.9 s, the prime included.

## The first move through the service

A client connected the wheel of a service just started, with the wheel resting on slot
4. It wrote `Position = 0` and read `Position` back as fast as it could for 8 s. It then
moved the wheel to slot 4, reconnected it, and moved it to slot 2. Each build ran twice,
each run a fresh service process.

| Build | First move, `Position` write | `Position` after it | Second move, write | First move after the reconnect, write |
|---|---|---|---|---|
| `317b2ccf` | 0.021 s | slot 0 from 0.122 s on (2 of 2) | 0.021–0.022 s | 0.021 s |
| `75837229` | 0.774 s | −1 until 4.41 s, slot 0 from 4.68 s (2 of 2) | 0.021 s | 0.773–0.774 s |

On `317b2ccf` the wheel still had about 3.9 s to travel when `Position` named slot 0,
and from then on `Position` was served from the cache. On `75837229` the write carries
the prime: the command to slot 4, one status read naming slot 4, the 250 ms rest (FW5)
and the command to slot 0. The slot-0 reading came 3.9 s after the command that set
the wheel off, which is the time it takes to travel there.

## The measurements

### How they were made

`cfw_probe` runs one mode per process. Each process opens the camera and asserts a CFW
with more than five slots is plugged in. It runs the handshake's init sequence (stream
mode, readout mode 0, `InitQHYCCD`) and then the mode. Every SDK call is one JSON line.
On Windows, the camera service on the host was stopped for the runs and started again
after them. Each run found the camera and its wheel again.

### 1. What the status names while the wheel moves

Each move was checked from its command to the read that named its target at the
at-rest pace, or to the next command for a move that never arrived.

| Setup | Moves | What the status named |
|---|---|---|
| QHY178M, Linux, 26.06.04: today's 13:19 PDT runs and the 35 processes of the [2026-10-10 wheel record](../2026-10-10-qhy-camera-qhy178m-cfw-linux-wheel/README.md) | 100, the dropped ones included | the slot commanded before the move (54), or slot 0 in a process's first move (46) |
| QHY178M, Windows VM, 26.06.04 | 15 that travelled, 3 dropped | `N` (0x4E) on every read of the 15, 398 reads |
| QHY178M, Windows VM, 26.07.28 | 15 that travelled, 3 dropped | `N` on every read of the 15, 383 reads |
| QHY600M, rig2, 24.01.09 | 29 that travelled, 4 dropped | `N` on every read of the 29, 740 reads |
| QHY600M, rig2, 26.07.28 | 29 that travelled, 4 dropped | `N` on every read of the 29, 730 reads |

A dropped move never set off, and its reads named the slot the wheel stayed on (§4); on
Linux that was also the slot commanded before it. Wherever a Linux move named the slot
the wheel had left, that slot was also the previous command. The move after a dropped
one tells the two apart: it named the dropped slot, where the wheel had never been
([wheel record §3](../2026-10-10-qhy-camera-qhy178m-cfw-linux-wheel/README.md#3-what-a-dropped-move-leaves-behind)).
The same camera, wheel and SDK version answered `N` under Windows, so naming a slot in
transit is the Linux SDK build's doing.

`GetQHYCCDCFWStatus`, the SDK's other status call, gave the Linux runs no other signal.
Through a wrapper that was not committed, it returned one byte, the same as the
`CONTROL_CFWPORT` read beside it, slot 0 included, throughout a process's first move
and the move after it (2 of 2).

### 2. A process's first move to slot 0, and the prime

`cfw_probe zero` sends a process's first move from slot 3 to slot 0 and reads on for 8 s
after the status names slot 0. `cfw_probe same` first sends the wheel to slot 3, the
slot it stands on, reads for 10 s, rests 2 s and then makes the same move.

| Setup | `zero`: slot 0 named | `same`: after the command to slot 3 | `same`: slot 0 named |
|---|---|---|---|
| QHY178M, Linux | at 120.5 and 120.6 ms; travelling-pace reads went on to ≈3.4 s | slot 3 named by the first read, which took 482 ms; then 40 reads over 10.2 s, every one at the at-rest pace naming slot 3 (2 of 2) | at 3.857 s; slot 3 named in transit (2 of 2) |
| QHY178M, Windows VM, each SDK | at 3.84–3.86 s, `N` before (2 of 2) | not run | not run |
| QHY600M, rig2, each SDK | at 4.00–4.02 s, `N` before (2 of 2) | slot 3 named by the first read, which took 455–476 ms; then 38 reads over 10 s, every one at the at-rest pace naming slot 3 (2 of 2) | at 4.01–4.03 s, `N` before (2 of 2) |

### 3. What a re-init or a re-open keeps

`cfw_probe forget <how>`: the wheel is sent to slot 2, and after a re-init, a close and
re-open with its init, or neither, to slot 4. On Linux every move to slot 4 named slot 2
in transit (none 1 of 1, re-init 2 of 2, re-open 2 of 2): the SDK keeps the slot it was
last sent through both. On rig2 each read `N` (1 of 1 for each `how`, with each SDK);
the VM ran `none` only, which read `N` too.

### 4. The drop (FW5) on Windows

`cfw_probe rest <ms>` and `lost <then>` as in the
[wheel record](../2026-10-10-qhy-camera-qhy178m-cfw-linux-wheel/README.md#1-a-move-straight-after-the-read-that-saw-the-last-one-arrive-fw5).

| Setup | Sent 0 ms after the arrival read | Sent 15 ms after | Sent after a 2 s rest and an idle read |
|---|---|---|---|
| QHY178M, Windows VM, each SDK | dropped, 3 of 3 | not run | not run |
| QHY600M, rig2, each SDK | dropped, 4 of 4 | arrived, 1 of 1 | arrived, 1 of 1 |

A dropped move read the slot the wheel stayed on, at the at-rest pace, from the first
read. After it, a resend of the dropped slot travelled with `N` in transit and arrived
in 2.69 s on the QHY178M and 2.79 s on the QHY600M: two slots' travel (1 of 1 for each
setup). Under Linux, a resend in the same situation names its target from the first
read.

### 5. Homing on the QHY600M (C5)

`cfw_probe home 3` on rig2, with each SDK: the wheel was brought to slot 3, then re-inited
twice on the open handle, closed, re-opened and inited. After each step the status named
slot 3 on every read, at the at-rest pace: 31, 31, 11 and 31 reads with 24.01.09, and 31,
31, 12 and 31 with 26.07.28.

Power does home the wheel. The dev box's wheel, left at slot 5 and later at slot 4 with
its 12 V switched off, named slot 0 when the 12 V came back, on the first read of the
first process, before anything had sent it anywhere.

### 6. The QHY178M after the VM

The camera was attached to the VM once, used for both Windows SDKs, and detached. Back on
Linux, its first 1 ms exposure took 62 s to come back, successfully, with no USB error in
the kernel log. The next took 2.4 s, the usual time, and the camera was used normally
from then on.

## Afterwards

The cooler was never engaged. The dev box's services were stopped and the Quad 12V
output switched off; the VM was shut down. On rig2 the camera service was started again
after each set of runs and found its camera and wheel (`cameras=1 filter_wheels=1`).

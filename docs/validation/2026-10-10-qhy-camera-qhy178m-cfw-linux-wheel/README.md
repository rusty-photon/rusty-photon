# qhy-camera on Linux: QHY178M + CFW, 2026-10-10 (a wheel move rests after a status read)

This is a recorded Linux ConformU run against the same physical QHY178M + CFW as the
[2026-10-10 cooler record](../2026-10-10-qhy-camera-qhy178m-cfw-linux-cooler-report/README.md).
It covers the change that makes a filter-wheel move wait 250 ms after the wheel's last
status read ([FW5](../../services/qhy-camera.md#filterwheel-when-a-cfw-is-detected),
PR #1452).

Next to the ConformU run are the measurements behind three statements in the design
doc:
- FW5, the dropped move;
- FW6, the camera's traffic during the wheel's travel;
- C5, which no longer says a connect homes the wheel.

They were made with `qhyccd-rs`'s new hardware probe,
[`examples/cfw_probe.rs`](../../../crates/qhyccd-rs/examples/cfw_probe.rs).

**The wheel is not in the camera's light path on this rig.** Nothing below is optical.
Everything is what the CFW reports, the slot `CONTROL_CFWPORT` names, and how long
each status read takes. A status read takes ≈255 ms when the wheel is at rest and
≈100 ms while it travels, and that pace is the only sign of motion used here. Where
the wheel physically stands can only be inferred from travel times: one slot takes
≈1.5 s, two ≈2.7 s, three ≈3.9 s.

## What was tested

| | |
|---|---|
| Commit | [`8d28c795`](https://github.com/rusty-photon/rusty-photon/commit/8d28c795) (branch `fix/qhy-camera-connect-owns-device`, PR #1452), on `main` at `7fdbf4e6` |
| Service | `qhy-camera`, **real-SDK** build (default features), `cargo run -p qhy-camera -- --log-level debug` (dev profile); rustc 1.99.0 (b940084d7 2026-09-28) |
| Probe | `cfw_probe`, `cargo build -p qhyccd-rs --example cfw_probe` (dev profile), from the source in that commit; run with the service stopped, since it opens the camera itself |
| SDK | QHYCCD SDK **26.06.04**: `/usr/local/lib/libqhyccd.so` → `libqhyccd.so.26.6.4.16`, sha256 `f51b92f9189fae7707e98ad334cf52d3c1493a6485f33394b39a18a3f4d5c738` (byte-identical to every earlier QHY record) |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.8-200.fc44 |
| Camera | QHY178M-Cool (USB `1618:c179`, SuperSpeed), SDK id `QHY178M-222b16468c5966524`, with its 7-slot CFW3. It sits on USB3 port 3 of the Pegasus PPBA Gen2's embedded hub, and its 12 V comes from the PPBA's Quad 12V output, driven through `ppba-driver` (`cargo run -p ppba-driver`) |
| ConformU | **4.5.0** build 53834.49ab847 (the latest release), against `http://127.0.0.1:11121/api/v1/{camera,filterwheel}/0` |

The Quad 12V output was switched on before anything below ran. The service enumerated
both devices on each start (`cameras=1 filter_wheels=1`). Before ConformU, three 10 ms
light frames were taken, each ready in 2.6 s, and the back-to-back client below ran.
The SDK ran on its defaults: no `qhyccd.ini` in the working directory.

## Verdicts

| Device | Suite | Result |
|---|---|---|
| Camera | `alpacaprotocol` | 0 errors, 0 issues, 16 information messages ([log](alpacaprotocol-camera.log)) |
| Camera | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-camera.log), [results](conformance-camera-results.json)) |
| FilterWheel | `alpacaprotocol` | no errors, issues or information alerts ([log](alpacaprotocol-filterwheel.log)) |
| FilterWheel | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-filterwheel.log), [results](conformance-filterwheel-results.json)) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount`
are all **0** in both results files. The 16 informational items are the set every QHY
record carries. ConformU's `Position` writes returned in 0.021–0.023 s. It waits about
a second after each arrival before its next move, so the rest had always run out
before its writes arrived, and its runs never met the drop.

## The measurements

### How they were made

`cfw_probe` runs one mode per process. Each process opens the camera and asserts a CFW
is plugged in with more than five slots. It then runs the camera handshake's init
sequence (stream mode, readout mode 0, `InitQHYCCD`), as a connect does, and then the
mode. Every SDK call is one JSON line, timestamped in ms. 35 processes ran in a row,
the wheel left wherever the last one put it. Every process found the wheel naming the
slot the process before had left it on, 34 of 34.

The same edges were first found earlier the same day, in an exploratory session with
an earlier revision of the probe. A move sent 0–10 ms after the arrival read was
dropped 12 times of 12. Sent 15 ms to 2 s after it, the move arrived 18 times of 18.
The tables below are the committed probe's own runs.

### 1. A move straight after the read that saw the last one arrive (FW5)

`cfw_probe rest <ms>`: the wheel travels to slot 2 and is read until it names it.
After `<ms>` more, it is sent to slot 4 and read for 8 s.

| Rest after the arrival read | Moves | Reached slot 4 |
|---|---|---|
| 0 ms | 4 | 0 |
| 5 ms | 1 | 0 |
| 10 ms | 2 | 0 |
| 15 ms | 2 | 2, in 2.69 s |
| 20 ms | 2 | 2, in 2.69 s |
| 2 s, then one idle read straight before the move | 2 | 2, in 2.69 s |

Each dropped move went the same way. From the command on, every read took the at-rest
≈255 ms and named slot 2 for the whole 8 s, so the wheel never set off.

### 2. The same, through the service

A client polled `Position` until it named the slot just commanded, then PUT the next
one at once, alternating slots 2 and 4.

| Build | Moves | Reached | `Position` PUT |
|---|---|---|---|
| `2b1f8154` (the PR before this change) | 1 | 0: `Position` read −1, the moving sentinel, for the 12 s watched | 0.02 s |
| `8d28c795` | 20 | 20, each in 2.94–3.29 s | 0.27 s: the 250 ms rest, then the command |

A different slot recovered the stranded wheel on `2b1f8154`. Its `Position` named slot 5
after ≈3.5 s, the travel from slot 2, where the wheel had stayed.

### 3. What a dropped move leaves behind

`cfw_probe lost <then>`: a move from slot 2 to slot 4 is dropped as in §1. Half a
second later the probe either sends slot 4 again or goes on to slot 5.

| Then | Result (2 of 2 each) |
|---|---|
| Slot 4 again | The status named slot 4 after 120.7 ms and 120.6 ms. That is one status read, with no travel: two slots take ≈2.7 s. The move to slot 5 after it read at the travelling pace for ≈1.8 s, then at the at-rest pace naming slot 4, and never named slot 5 in the 10 s watched. |
| Slot 5 | Reached in 3884 ms and 3883 ms, the travel for three slots, from slot 2. One slot, from slot 4, takes ≈1.5 s. In transit the status named slot 4, the dropped target. |

So a dropped move leaves the CFW holding the target it never travelled to. Re-sending
that slot gets an immediate "arrived", and the next move goes wrong. Commanding a
different slot moves the wheel properly from where it really stands. This is why the
driver does not re-send a commanded slot (FW2) and has no recovery of its own.

### 4. The camera's traffic during the wheel's travel (FW6)

`cfw_probe during <call> <ms>`: a rested wheel is sent from slot 2 to slot 4 (2.69 s),
and the camera call is made `<ms>` into the travel. `cfw_probe overlap <ms>`: the init
sequence starts, and the move is sent `<ms>` after it.

| Camera call | Into the travel | Reached slot 4 |
|---|---|---|
| none | 800 ms | 2 of 2 |
| `InitQHYCCD` | 800 ms | 2 of 2 |
| init sequence (stream mode, readout mode, `InitQHYCCD`) | 300, 1500, 2200 ms | 3 of 3 |
| readout-mode write | 800 ms | 1 of 1 |
| stream-mode write | 800 ms | 1 of 1 |
| ten `CurTemp` + `CurPWM` reads | 300 ms | 1 of 1 |
| a 1 ms exposure, downloaded | 800 ms | 1 of 1 |
| the move sent 0, 100, 200, 250, 300 ms into the init sequence | — | 5 of 5 |

Every arrival came at 2.69 s. Two runs read later: 3.00 s after the 2200 ms init
sequence and 3.45 s after the exposure. In those runs the call held the probe's only
thread past the arrival, and the first read after the call named the target.

`cfw_probe reads 0 50 150 250 500`: five inits, each started that many ms into 2 s of
back-to-back status reads on a second thread. All 5 inits succeeded, and all 40 reads
named the wheel's slot. Two inits took ≈320 ms instead of ≈74 ms: they started while a
read was in flight, and the SDK held them behind it.

### 5. A connect does not home the wheel (C5)

`cfw_probe home 3`: the wheel is brought to slot 3. The probe then makes two re-inits on
the open handle, and a close and re-open followed by an init. The status was read for
8 s after each re-init, and for 3 s and 8 s around the re-open.

| After | Reads | Named |
|---|---|---|
| re-init 1 | 32 | slot 3, every read at the at-rest pace |
| re-init 2 | 32 | slot 3, every read at the at-rest pace |
| close and re-open, before its init | 12 | slot 3, every read at the at-rest pace |
| its init | 32 | slot 3, every read at the at-rest pace |

A homing that came back to slot 3 would have shown as reads at the travelling pace;
there were none. The 34 fresh SDK starts in §1–§4 also found the wheel where the last
process had left it.

### 6. The first move after the SDK starts

In all 34 processes that moved the wheel, the status named slot 0 (`0x30`) for the
whole of the first move. That held whatever slot the move started from or went to. Every
later move in a process named the slot it had left until it arrived. A first move *to*
slot 0 therefore reads as arrived on its first read. The driver does not handle this yet
([Future Work](../../services/qhy-camera.md#future-work)).

## Afterwards

The cooler was never engaged. The services were stopped and the Quad 12V output
switched off.

# qhy-camera on Linux: QHY178M + CFW, 2026-10-10 (a wheel move's deadline)

This is a recorded Linux ConformU run against the dev box's QHY178M + CFW3, the camera
and wheel of the [2026-10-10 first-move record](../2026-10-10-qhy-camera-qhy178m-cfw-linux-first-move/README.md).
It covers two rules added to the service's
[filter wheel contract](../../services/qhy-camera.md#filterwheel-when-a-cfw-is-detected):
- **FW8:** a move that has not arrived 30 s after it was sent has failed, and `Position`
  reports it as error `0x500` until a write goes out to the wheel;
- **FW2:** a write of another slot while this connection's move is under way is refused
  with `INVALID_OPERATION`.

It also holds the measurements those rules rest on:
- how long the wheel travels between any two slots;
- what a connect does while the wheel travels;
- what a CFW does when its 12 V is cut mid-move and restored.

That last one is in C5, the service's connect contract.

As in the earlier records, nothing here is optical. The wheel is not in the camera's
light path, so all of it is what the CFW reports and how long each status read takes:
≈255 ms with the wheel at rest, ≈100 ms while it travels.

## What was tested

| | |
|---|---|
| Commit | [`6fd3a513`](https://github.com/rusty-photon/rusty-photon/commit/6fd3a513) (branch `fix/qhy-camera-wheel-move-deadline`), on `main` at `1c367361` |
| Service | `qhy-camera`, **real-SDK** build (default features), `cargo run -p qhy-camera -- --config <config> -l debug` (dev profile), config `{"server": {"port": 11121}}`; rustc 1.99.0 (b940084d7 2026-09-28) |
| Earlier heads | [`a5dc7daa`](https://github.com/rusty-photon/rusty-photon/commit/a5dc7daa) and [`944be766`](https://github.com/rusty-photon/rusty-photon/commit/944be766), run the same way; what they showed is under *Earlier heads* |
| Probe | `qhyccd-rs`'s [`cfw_probe`](../../../crates/qhyccd-rs/examples/cfw_probe.rs), modes `travel` and `powercut` as at `6fd3a513`. `travel` ran from the tree committed as `a5dc7daa`, unchanged since. `powercut`'s first run predates its optional command, the mode otherwise the same. `cargo build -p qhyccd-rs --example cfw_probe`, run with the service stopped |
| SDK | QHYCCD SDK **26.06.04**: `libqhyccd.so.26.6.4.16`, sha256 `f51b92f9189fae7707e98ad334cf52d3c1493a6485f33394b39a18a3f4d5c738` (byte-identical to every earlier QHY record) |
| Platform | Fedora Linux 44 (Workstation Edition) x86_64, kernel 7.2.8-200.fc44 |
| Camera | QHY178M-Cool, SDK id `QHY178M-222b16468c5966524`, with its 7-slot CFW3, powered through the PPBA's Quad 12V output |
| ConformU | **4.5.0** build 53834.49ab847, against `http://127.0.0.1:11121/api/v1/{camera,filterwheel}/0` |

The Quad 12V output was switched on about 35 s before each service start, and the
service enumerated both devices each time (`cameras=1 filter_wheels=1`). The SDK ran
on its defaults: no `qhyccd.ini` in any working directory. Times below are PDT.

## Verdicts

| Device | Suite | Result |
|---|---|---|
| Camera | `alpacaprotocol` | 0 errors, 0 issues, 16 information messages ([log](alpacaprotocol-camera.log)) |
| Camera | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-camera.log), [results](conformance-camera-results.json)) |
| FilterWheel | `alpacaprotocol` | no errors, issues or information alerts ([log](alpacaprotocol-filterwheel.log)) |
| FilterWheel | `conformance` | *"no errors, warnings or issues found"*; every member within its target response time ([log](conformance-filterwheel.log), [results](conformance-filterwheel-results.json)) |

`ErrorCount` / `IssueCount` / `ConfigurationAlertCount` / `TimingIssuesCount` are all
**0** in both results files. The 16 informational items are the set every QHY record
carries. ConformU's first wheel move, on a fresh connection, went
through FW7's prime and was written in 0.774 s; its other 19 moves were written in
0.001–0.021 s, and it found −1 and 7 rejected. None of its moves met the refusal: it
waits for each move to arrive before the next.

## How far the wheel travels

`cfw_probe travel` (16:30–16:38) rests the wheel 2 s before each move and times the move
from its command to the first status read that names its slot. Its first command goes to
the slot the wheel stands on, so from then on the Linux SDK's status names the slot each
move left until the wheel arrives (FW7). It moved from every slot to every other and back
again, 82 moves, all of which arrived:

| Slots travelled | Moves | Arrived after |
|---|---|---|
| 1 | 26 | 1.46–1.52 s |
| 2 | 28 | 2.66–2.74 s |
| 3 | 22 | 3.83–3.94 s |
| 4 | 6 | 5.05–5.08 s |

That is ≈1.2 s a slot. The wheel takes the shorter way round, except that slot 0 to 4,
1 to 5 and 2 to 6 go four slots forward (two each), where 3 to 0, 4 to 1, 5 to 2 and
6 to 3 go three slots back. The longest move, 2 to 6, took 5.08 s;
FW8's 30 s is six times that.

## Through the service, on `6fd3a513`

Run 17:10–17:14 by a script that writes and reads the wheel over Alpaca, connected with
the camera, the wheel resting on slot 0 at the start.

**A write while a move is under way (FW2).** The client wrote a slot and at once another.
The second write was refused in under 1 ms with `0x40B`, *"the filter wheel is still
moving to slot 4"* (and *"... slot 6"*). `Position` read −1 until the wheel named the
first slot, 6.27 s and 5.08 s after the runs began. The other slot, written again then,
went out after FW5's rest and arrived (2 of 2). A second write of the slot under way
was accepted and not sent again, and the wheel arrived (2 of 2).

**A connect while the wheel travels.** The client wrote a slot, released the wheel
0.8 s later, and asked to connect it again every 0.3 s. Every connect made while the
wheel travelled failed with `NOT_CONNECTED` after ≈0.1 s: 14 and 10 of them. The log
shows the slot-count read failing: `GetParameter { control: CfwSlotsNum }`. The first
connect after the wheel arrived succeeded and `Position` named the slot it had reached
(2 of 2). The earlier heads did the same over four more moves, 10, 14, 14 and 10 failed
connects, each move ending in a connect that succeeded. On one more move the client
connected only once, mid-travel, and that connect failed too. In all, 73 connects made
during 7 moves failed, and the first connect after each arrival succeeded (7 of 7).

**A move that does not arrive (FW8).** The client wrote a slot and cut the CFW's
12 V 1.8 s after the write began.

| | Cut 1 | Cut 2 |
|---|---|---|
| Write | slot 6, from slot 2 (primed, 0.775 s) | slot 2, from slot 6 (0.023 s) |
| 12 V cut | 1.8 s after the write began | 1.8 s |
| `Position` until the deadline | −1, at the travel pace | −1, at the travel pace |
| Failure reported | 30.95 s after the write began | 30.29 s |
| Message | *the filter wheel did not reach slot 6 within 30s; its status names slot 2* | *... slot 2 ...; its status names slot 6* |

Each failure was the first read after the deadline: 30.1–30.2 s and 30.2–30.3 s after
the slot was sent. The slot each status named is the slot sent before the move, the
Linux SDK's answer while the CFW does not report a slot at rest. From then on
`Position` answered the failure in ≤3 ms without reading the wheel, through the 12 V
coming back.

Written again about 7 s after the 12 V came back, while the CFW still homed (C5), the failed
slot was refused after 1.18 s (`INVALID_OPERATION`). Its prime went through slot 2, the
slot a fresh read named in transit, and the status did not confirm it. `Position` went on
answering the failure (1 of 1). Written again about 37 s and 30 s after the 12 V came back,
each went through slot 0, which a fresh read named at rest. The write returned in 1.28 s
and the wheel arrived at the failed slot 3.0 s and 4.2 s after the write began (2 of 2).
That includes the fresh read and FW5's rests.

## What a CFW does when its power comes back

`cfw_probe powercut` (17:02–17:07) sends a rested wheel to a slot. A script cuts the
12 V 1.0 s or 1.8 s into the move and restores it 8 s later. The probe reads the status
throughout.

| Run | Move | 12 V cut, restored | In transit until | Then |
|---|---|---|---|---|
| 1 | 6 to 2 | +1.0 s, +9.1 s | 19.5 s after the power came back | at rest once, in transit 2.1 s more, at rest on slot 0 from 21.9 s on |
| 2 | 0 to 4 | +1.8 s, +9.9 s | 17.4 s | the same; at rest on slot 0 from 19.9 s on |
| 3 | 0 to 3, then a command to slot 5 at +14.1 s | +1.0 s, +9.1 s | 18.3 s | the same; at rest on slot 0 from 20.7 s on |

While unpowered and while homing, every read took ≈100 ms and named the slot sent before
the latest command. In run 3 that was slot 0, and slot 3 once the command to slot 5 had
gone. The command to slot 5, sent 5 s into the homing, was dropped: the homing kept its
shape and the wheel came to rest on slot 0, where it read for the next 29 s.

## Earlier heads

The run on `a5dc7daa` (16:48–16:52) refused writes under way, failed the move at its
deadline after a 12 V cut, and failed reconnects during travel, all as above. Its
failed move then left the slot its last read named, slot 6, as where the wheel stood.
The failed slot, written again 7 s after the power came back, was primed through slot 6
and refused. That write came during the homing, so it says nothing about whether such a
prime moves the wheel. But the wheel was homing to slot 0, and a wheel at rest on slot 0
sent to slot 6 travels there. So from `944be766` on, a failed move leaves no slot held,
and the prime reads the status afresh.

The run on `944be766` (16:58–17:01) made the same writes. Its first re-send, 20 s after
the power came back, read slot 0 afresh and arrived. Its second, 19.5 s after, met the
homing and was refused. That head had cleared the failure on the refused write, so
`Position` then took slot 2 from a read at the travel pace, the slot sent before the
move, as where the wheel rested. A later write of slot 2 would not have been sent. So
from `6fd3a513` on, only a write that goes out to the wheel ends a failed move's report.

## After the run

The service was stopped and the 12 V switched off at 17:17. The wheel was left on
slot 0, where ConformU's last move had put it.

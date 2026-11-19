# qhy-camera on the field rig — QHY5III715C guide camera, an hour unbinned, Linux, 2026-11-18

A sustained-cadence measurement with no ConformU run, made for stage G1 of
the [native guider plan](../../plans/native-guider.md#staging). 2 s
exposures of the QHY5III715C's full frame, unbinned, were looped back to
back through the standard Alpaca Camera API for an hour, timing how long
after each exposure ends the pixels are in the client. The guider reads
this colour sensor unbinned and sums its 2 × 2 cells itself
([guider.md § Sensor handling](../../services/guider.md#sensor-handling)).
The camera's [2026-11-17 record](../2026-11-17-qhy-camera-qhy5iii715c-linux/README.md)
holds its ConformU run and an hour of frames the SDK binned 2 × 2, but only
ten minutes of the unbinned frame, its run C. This run is the unbinned frame
for the hour.

**The unbinned frame holds the cadence for the hour.** 1370 frames arrived
with no failure, every one of them 0.62–0.66 s after its exposure ended
(median 0.62 s, 95th percentile 0.65 s), against a 2 s exposure. The
figures match run C's to the millisecond, so the ten minutes showed the
steady state.

## What was tested

| | |
|---|---|
| Commit | [`283138ae`](https://github.com/rusty-photon/rusty-photon/commit/283138ae668ab6bb5df5c491fd79727cb2b20b84) (`main`), published as nightly `0.1.0+nightly.202611091347.g283138a`: the build of the 2026-11-17 record, unchanged on the rig since then |
| Service | The packaged arm64 nightly deb `rusty-photon-qhy-camera 0.1.0+nightly.202611091347.g283138a`; the unit had been running since 2026-11-09. QHY SDK 26.06.04, built into the binary |
| Platform | Raspberry Pi 5 Model B Rev 1.1 (aarch64), Raspberry Pi OS (Debian 13 trixie), the telescope field rig, as in the 2026-11-17 record |
| Camera | QHY5III715C, ASCOM `UniqueID` `QHY5III715C-6cf42c41a8b2f3188`, device 0 of `qhy-camera`. Gain 30, offset 30, readout mode 0 (`STANDARD MODE`), read into [`D/meta.json`](D/meta.json) |
| Endpoint | The service's production endpoint, port 11121, over loopback: TLS with the rig's certificate, verified by name, and HTTP Basic auth with the credential in `rp`'s configuration |
| Operations | Run at the owner's word. No session was running. The runner stopped `rp` for the hour, so nothing else used the camera, and started it again at the end. The mount was at rest at its park pose, with tracking off |

Times in the logs are UTC; the rig's local time is Pacific Standard Time
(UTC−8):

| Step | Local time | UTC |
|---|---|---|
| `rp` stopped | 07:47:24 | 15:47:24 |
| Run D | 07:47:24–08:47:25 | 15:47:24–16:47:25 |
| `rp` started again | 08:47:25 | 16:47:25 |

## Method

The method of the 2026-11-17 record
([§ The cadence runs](../2026-11-17-qhy-camera-qhy5iii715c-linux/README.md#the-cadence-runs)),
with that record's
[`cadence.py`](../2026-11-17-qhy-camera-qhy5iii715c-linux/cadence/cadence.py),
[`alpaca_client.py`](../2026-11-17-qhy-camera-qhy5iii715c-linux/cadence/alpaca_client.py)
and [`summarize.py`](../2026-11-17-qhy-camera-qhy5iii715c-linux/cadence/summarize.py),
unchanged: each copy on the rig had the SHA-256 of the record's. Per frame,
`t_req` just before `PUT startexposure` (`Duration=2`, `Light=true`), the
first `true` from `GET imageready` polled every 25 ms, and `t_pixels` when
the whole ImageBytes body of `GET imagearray` has been read;
**`latency = t_pixels − (t_req + 2 s)`**. Every frame's ImageBytes header is
checked.

[`cadence-d.sh`](cadence-d.sh), run as root under `systemd-run`, stops
`rp`, runs `cadence.py --label D --bin 1 --frame full --seconds 3600 --cpu`,
and starts `rp` again however the run ends.

## Results

| Run | Frame | Frames | Failures | Frames / hour | Bytes per frame |
|---|---|---|---|---|---|
| **D** | full frame, bin 1: 3864 × 2192, 1 h | 1370 | 0 | 1369 | 16,939,820 |

Milliseconds, minimum / median / 95th percentile / maximum:

| | `latency` | `ready_after_end` | `download` | `cycle` | `start_call` |
|---|---|---|---|---|---|
| **D** | 616 / **623 / 652 / 664** | 439 / 439 / 440 / 465 | 177 / 184 / 212 / 224 | 2616 / 2623 / 2652 / 2664 | 33 / 33 / 34 / 34 |
| 2026-11-17 run C, 10 min | — / 623 / 653 / 664 | — / 439 / 440 / 442 | — / 184 / 213 / 225 | — / 2623 / 2653 / 2664 | — / 33 / 34 / 37 |

Every frame's `latency` was over 500 ms, and none was over 1000 ms. As in
the 2026-11-17 record, `latency` is measured from the request, so it
includes the 33 ms `StartExposure` call before the exposure starts.

Host load, one line a minute in [`D/cpu.csv`](D/cpu.csv):
- the whole host 2.8–3.0 % busy over its four cores;
- `qhy-camera` 8.3–8.9 % of one core, its resident memory 182 MB all hour;
- load average at most 0.31, and the SoC at 42.5–48.5 °C.

`qhy-camera`'s journal for the hour holds no warning and no error.

## Files

- [`cadence-d.sh`](cadence-d.sh): the runner, as run.
- [`run.out`](run.out): the runner's log, with `rp`'s state at the start,
  the stop, the run's exit and `rp`'s state at the end.
- [`D/`](D/): [`run-D.csv`](D/run-D.csv), every frame;
  [`summary-D.md`](D/summary-D.md) and [`.json`](D/summary-D.json);
  [`meta.json`](D/meta.json), the camera's settings read at the start; and
  [`cpu.csv`](D/cpu.csv). The CSVs are stored with LF line endings; the
  script wrote CRLF.
- No file names the rig: the script writes the endpoint into `meta.json`
  with the placeholder `<rig-host>`, and nothing else carries a host, an
  address, a user name or a credential.

## State the rig was left in

- **Camera:** connected, at bin 1, full frame, gain 30, offset 30: the
  settings it had before the run.
- **`rp`:** started again at 16:47:25 UTC; it bound its 11 roster entries,
  as at its previous start, and logged no warning or error.
- **Mount:** at rest at its park pose, tracking off, as before the run.
  `AtPark` reads false, as it has since the driver restarted at 05:22 UTC
  during the 2026-11-17 session.
- **Rig:** the runner and its outputs stay in the operator's home. No
  configuration file was changed.

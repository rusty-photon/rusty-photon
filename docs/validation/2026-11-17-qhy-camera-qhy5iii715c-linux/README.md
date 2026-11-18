# qhy-camera on the field rig — QHY5III715C guide camera, Linux, 2026-11-17

The first `docs/validation/` record of the field rig's guide camera, the
QHY5III715C, under `qhy-camera`, made for stage G1 of the
[native guider plan](../../plans/native-guider.md#staging). It holds the
camera's ConformU run and the first sustained-cadence measurement of any
camera service: 2 s exposures looped back to back through the standard
Alpaca Camera API for an hour, timing how long after each exposure ends the
pixels are in the client. (The camera's
[2026-10-13 record](../2026-10-13-phd2-guider-qhy5iii715c-linux/README.md)
is of another service, `phd2-guider`'s camera facade, with PHD2 holding
the camera.)

**This is a scoped record, accepted limitation, filed by decision.** The
[all-zero rule](../../skills/hardware-validation.md) is not met:
`conformance` reports **10 issues**, 0 errors, 0 configuration alerts and 0
timing issues. All ten are one finding: the camera's valid bins are 1, 2
and 4, `MaxBinX` reports 4, and ConformU's exposure ladder and its four
bad-geometry tests at bin 3 report the `InvalidValue` the driver answers to
`BinX = 3`. That is
[#933](https://github.com/rusty-photon/rusty-photon/issues/933), closed as
*not planned* on 2026-08-08
([closing decision](https://github.com/rusty-photon/rusty-photon/issues/933#issuecomment-5227798512)).

- **What it accepts:** `MaxBinX` and `MaxBinY` report the largest bin the
  camera supports, and a bin below it that the camera does not support is
  refused with `InvalidValue`, so the camera's working 4 × 4 stays visible
  to clients that build their list from `MaxBinX`.
- **The consequence:** no clean two-suite record is planned for this
  camera. Reopening #933 makes one owed again.
- **Not the behaviour under test:** the cadence runs use bins 2 and 1.
  Bin 3 is in none of them.
- **The decision to file:** recorded on #933, the issue that carries the
  finding, since no issue asked for the run (the plan's stage G1 did):
  [#933 comment](https://github.com/rusty-photon/rusty-photon/issues/933#issuecomment-6970078706).

**The cadence meets G1's pass criterion.** Over the hour of run A (full
frame, binned 2 × 2), 1435 frames arrived with no failure, every one of
them 0.47–0.55 s after its exposure ended (median 0.51 s, 95th percentile
0.54 s), against a 2 s exposure.

## What was tested

| | |
|---|---|
| Commit | [`283138ae`](https://github.com/rusty-photon/rusty-photon/commit/283138ae668ab6bb5df5c491fd79727cb2b20b84) (`main`), published as nightly `0.1.0+nightly.202611091347.g283138a`. `main` at [`eae05abf`](https://github.com/rusty-photon/rusty-photon/commit/eae05abf) has four later commits to `qhy-camera`, `qhyccd-rs` and `rusty-photon-camera-core`: comments (#1783), the SDK simulator (#1788), BDD tests (#1743), and the restart route in front of the Alpaca router (#1734). None of them changes the exposure path, so the camera behaves the same on `main` |
| Service | The packaged arm64 nightly deb `rusty-photon-qhy-camera 0.1.0+nightly.202611091347.g283138a`, installed with the whole fleet on 2026-11-09; the unit had been running since then. Not a source build. QHY SDK 26.06.04, the version that commit pins, built into the binary (`ldd` lists no `libqhyccd`); libusb 1.0.28 |
| Platform | Raspberry Pi 5 Model B Rev 1.1 (aarch64), Raspberry Pi OS (Debian 13 trixie), kernel `6.18.34+rpt-rpi-2712`. This is the telescope field rig |
| Camera | QHY5III715C (Sony IMX715, colour), USB `1618:0716` after its firmware load, on a Pi USB 3 port at 5 Gb/s. ASCOM `UniqueID` `QHY5III715C-6cf42c41a8b2f3188` (model and SDK serial). Device 0 of `qhy-camera`, the only camera that process serves (`"devices": {}`). 3864 × 2192 at 1.45 µm, `SensorType` 2 (RGGB), `BayerOffsetX/Y` (0, 1), `MaxBinX` 4 with bins 1, 2 and 4, one readout mode (`STANDARD MODE`), gain 0–240, offset 0–205, `MaxADU` 65535 |
| Endpoint | The service's production endpoint: port 11121, TLS with the rig's Let's Encrypt wildcard certificate, HTTP Basic auth with the observatory credential. The rig's host name for the service resolves to loopback on the rig, so every client below reached it over loopback |
| Operations | `rp` was stopped from 03:47 UTC to the end of the session (one orchestrator at a time), so nothing else used the camera: Sentinel monitors no device here, and `ui-htmx` reaches devices only through `rp`. No session runner is installed on the rig, and PHD2 was not running |
| ConformU | **4.5.3** build 56334 (`33d9e18`), the release GitHub named latest that day, `linux-arm64`, downloaded to the rig for this run and checked against the SHA-256 GitHub gives for the asset. It ran on the rig as a transient systemd unit. The settings files were written from the client credential in `rp`'s configuration into a root-only directory and deleted when the run ended. Each held `{"SettingsCompatibilityVersion": 1}` and an `AlpacaConfiguration` block, and nothing else: every test group ran and every tolerance and delay was ConformU's default |
| `conformance` | Against the production endpoint, `https://<rig-host>:11121/api/v1/camera/0`, with `AccessServiceType` `Https`, the credential, and `TrustUserGeneratedSslCertificates` `true`. That last setting is needed with 4.5.3, which [cannot pass the certificate's name check](#conformu-453-and-the-certificate-name) |
| `alpacaprotocol` | Through the loopback header-injecting proxy of the [2026-08-08 record](../2026-08-08-svbony-camera-sv605cc-rig/README.md) on `127.0.0.1:18121`, with `AccessServiceType` `Http` and the credential, so its raw protocol requests authenticate end to end. The proxy adds the header only to a request without one, ConformU's embedded client-library calls, and forwards over TLS with the certificate verified by name |

Times in the logs are the rig's local time, Pacific Standard Time (UTC−8),
on 2026-11-17:

| Step | Local time | UTC (2026-11-18) |
|---|---|---|
| `alpacaprotocol` | 19:52:45–19:53:45 | 03:52:45–03:53:45 |
| `conformance` | 19:53:45–19:54:03 | 03:53:45–03:54:03 |
| Cadence run A | 19:56:02–20:56:04 | 03:56:02–04:56:04 |
| Cadence run B | 20:56:04–21:06:06 | 04:56:04–05:06:06 |
| Cadence run C | 21:06:06–21:16:08 | 05:06:06–05:16:08 |

## Verdicts

- **`alpacaprotocol`:** *"Found 0 errors, 0 issues and 16 information
  messages"* ([alpacaprotocol.log](alpacaprotocol.log)). The 16 are the
  set every QHY record carries: four casing variants each against
  `ImageArray`, `ImageArrayVariant`, `LastExposureDuration` and
  `LastExposureStartTime`, answered "no image available" or "value not
  set", because the suite connects, and a connect clears the exposure
  state, before it asks.
- **`conformance`:** **0 errors, 10 issues, 0 configuration alerts, 0
  timing issues**, and all 71 timing entries inside their target
  ([conformance.log](conformance.log), machine-readable
  [conformance-results.json](conformance-results.json)). ConformU took a
  2 s full frame at bins 1, 2 and 4 (3864 × 2192, 1932 × 1096 and
  966 × 548). Each reported `ImageReady` 2.51 s after `StartExposure`
  returned, and the bin-1 frame was read as an `ImageArray` in 242 ms.
  Gain and offset were read, written at their minimum and maximum, and
  refused one beyond each.

The ten issues are `StartExposure` and the `Reject Bad XSize`, `YSize`,
`XStart` and `YStart` tests at bin 3 × 3, each once for `BinX` and once for
`BinY`: *"Unable to set BinX to 3: bin 3 is not a supported binning
mode"*. ConformU's own `BinX Write` test logs the same refusal as
information (log lines 65–66).

### ConformU 4.5.3 and the certificate name

ConformU 4.5.3's first `conformance` attempt, with `AccessServiceType`
`Https` and no other connection setting, stopped at connect with
*"The remote certificate is invalid according to the validation
procedure: RemoteCertificateNameMismatch"*. The certificate is a wildcard
that covers the service's host name, and Python's `ssl` verified it under
that name. Two probes on the rig located the cause:

- A loopback listener that logs the server name in each TLS ClientHello
  saw ConformU 4.5.0 send the service's host name, and 4.5.3 send none.
  4.5.3 connects to the address the name resolves to, so the name check
  has nothing to match.
- ConformU 4.5.0, with the same settings file, connected and validated
  the certificate.

`TrustUserGeneratedSslCertificates` makes ConformU accept the certificate
without the name check. Every request was still TLS with Basic auth, to
the production endpoint over loopback. The cadence script and the
`alpacaprotocol` proxy both verified the certificate by name.

## The cadence runs

### Method

[`cadence/cadence.py`](cadence/cadence.py), run on the rig as root under
`systemd-run` by [`cadence/cadence-all.sh`](cadence/cadence-all.sh), with
[`cadence/alpaca_client.py`](cadence/alpaca_client.py) and
[`cadence/summarize.py`](cadence/summarize.py). It is Python's standard
library only.

- **Endpoint:** the production endpoint over loopback. The client opens TCP
  to `127.0.0.1:11121` and runs TLS with the server name and certificate
  verification of the host name `rp` uses for the camera, against the
  system trust store. Verification is on. The credential is read from
  `rp`'s configuration. One keep-alive connection carries every request.
- **Per frame:** `t_req` just before `PUT startexposure` (`Duration=2`,
  `Light=true`), `t_started` just after its answer; `GET imageready`
  polled every 25 ms from `t_started`, `t_ready` when the `true` answer
  arrived; `GET imagearray` with `Accept: application/imagebytes`, the
  whole body read, `t_pixels`. Derived: `start_call = t_started − t_req`,
  `ready_after_end = t_ready − (t_req + 2 s)`,
  `download = t_pixels − t_ready`, **`latency = t_pixels − (t_req + 2 s)`**,
  `cycle = t_pixels − t_req`. The next frame's request follows at once.
  The clock is `time.perf_counter_ns()`, with one UTC wall-clock stamp per
  frame.
- **Checks:** every frame's ImageBytes header (metadata version 1, error
  number 0, rank 2, dimensions `NumX` × `NumY`) and its content type. A
  failed frame would be a row with the error text, followed by a 1 s wait;
  none failed.
- **Camera settings:** the frame (`BinX`, `BinY`, `StartX`, `StartY`,
  `NumX`, `NumY`) is set before each run. Gain, offset and readout mode
  are left at the camera's values, read into each run's `meta.json`.
  The camera read gain 30 and offset 30 before the session. ConformU's
  suites write both, so the session set 30 and 30 again before run A, in
  the first of three smoke tests of three frames each at 03:55 UTC, which
  are not part of the runs.
- **Host load:** `cpu.csv` in each run's directory, one line a minute:
  load average, the whole host's busy share, `qhy-camera`'s and the
  script's CPU share of one core, `qhy-camera`'s resident memory, the SoC
  temperature.

Because `latency` and `ready_after_end` are measured from the request, they
include the `StartExposure` call (33 ms) before the exposure starts, and
overstate the latency by up to that much. The 25 ms poll puts each
`t_ready` on the poll's grid, so `ready_after_end` is good to 25 ms: in
run A it reads one of two values, 440 or 465 ms.

### Results

| Run | Frame | Frames | Failures | Frames / hour | Bytes per frame |
|---|---|---|---|---|---|
| **A** | full frame, bin 2: 1932 × 1096, 1 h | 1435 | 0 | 1434 | 4,234,988 |
| **B** | 512 × 512 at (1676, 840), the sensor's centre, bin 1, 10 min | 265 | 0 | 1585 | 524,332 |
| **C** | full frame, bin 1: 3864 × 2192, 10 min | 229 | 0 | 1370 | 16,939,820 |

Milliseconds, median / 95th percentile / maximum:

| Run | `latency` | `ready_after_end` | `download` | `cycle` | `start_call` |
|---|---|---|---|---|---|
| **A** | **507 / 542 / 546** | 464 / 465 / 467 | 67 / 80 / 86 | 2507 / 2542 / 2546 | 33 / 34 / 36 |
| **B** | 270 / 271 / 313 | 264 / 265 / 265 | 6 / 6 / 49 | 2270 / 2271 / 2313 | 33 / 34 / 34 |
| **C** | 623 / 653 / 664 | 439 / 440 / 442 | 184 / 213 / 225 | 2623 / 2653 / 2664 | 33 / 34 / 37 |

Share of frames with `latency` over 500 ms: A 72.8 %, B 0 %, C 100 %.
Over 1000 ms: none in any run. The per-run summaries are
`cadence/<run>/summary-<run>.md` and `.json`, and every frame is in
`cadence/<run>/run-<run>.csv`.

Host load during run A: whole-host busy 1.5–2.0 % of four cores,
`qhy-camera` 3.6–4.1 % of one core, load average at most 0.49, SoC
42.5–46.3 °C, `qhy-camera` resident memory 134 MB rising to 150 MB in the
first 14 minutes and flat after. Run C, full frames unbinned:
`qhy-camera` 8.4–8.8 % of a core, 182 MB.

### What they show

- **Run A meets the pass criterion.** Exposure end to pixels in the
  client is about a quarter of the exposure, frame after frame for the
  hour: no frame took more than 0.55 s, and none failed. The loop
  delivers a 2 s frame every 2.51 s.
- **The time is the camera's, before `ImageReady`.** Of A's 0.51 s,
  0.44–0.46 s pass before `ImageReady` turns true; the download of the
  4.2 MB frame over loopback TLS takes 28–86 ms, in two modes, near 30 ms
  and near 70 ms; the slower one held most of the hour.
- **The time before `ImageReady` depends on the frame, not the bin.**
  It is 0.26 s for the 512 × 512 subframe and 0.44 s for the full frame
  at bin 1, and the full frame at bin 2 takes no less than at bin 1.
  The subframe is the cheapest frame by far: 0.27 s from exposure end to
  pixels, 6 ms of it the download.
- **The download grows with the frame.** The full 17 MB frame takes
  0.18–0.22 s. A full unbinned frame reaches the client 0.62–0.66 s after
  its exposure ends.
- **Single-shot timing agrees.** In this record's ConformU run,
  `ImageReady` came 2.51 s after `StartExposure` returned for a 2 s
  frame at every bin. The 2.5–4.5 s quoted from earlier single-shot runs
  on other hosts does not appear on this camera, this driver build and
  this host.

## Files

- [`alpacaprotocol.log`](alpacaprotocol.log): unmodified in content. The
  device under test appears as the loopback proxy (`127.0.0.1:18121`).
  ConformU wrote CR characters inside its multi-line `Response:` entries;
  the repository's `.gitattributes` stores every file with LF line endings,
  so those CRs are not in the committed copy.
- [`conformance.log`](conformance.log): unmodified **except lines 5 and 7**,
  where the rig's host name is replaced by the placeholder `<rig-host>`.
- [`conformance-results.json`](conformance-results.json): unmodified. It
  carries no host.
- [`cadence/`](cadence/): the script, its client, its summariser and the
  runner, and for each run its CSV, summary, `meta.json` and `cpu.csv`.
  The CSVs are stored with LF line endings; the script wrote CRLF.

## State the rig was left in

- **Camera:** connected, as `rp` had it, at bin 1, full frame, gain 30,
  offset 30, readout mode 0, the settings it had before the session.
- **`rp`:** started again at 05:22 UTC; it bound every roster entry, the
  camera and the mount among them.
- **Mount:** at rest at its park pose, tracking off, as before the session.
  The GTi pulse timing that followed the cadence runs is in
  [star-adventurer-gti.md](../../services/star-adventurer-gti.md#pulseguide-lifecycle).
- **Rig:** ConformU 4.5.3 stays installed in the operator's home for later
  runs. No configuration file was changed, and the temporary settings
  files and credential files were deleted.

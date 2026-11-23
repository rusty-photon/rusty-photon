# guide-replay on rig2: an hour of the QHY5III678M, recorded and replayed, 2026-11-22

The native guider's stage G3
([the plan](../../plans/native-guider.md#staging), decision 15) records
an hour of guide-camera frames on sky, with the mount tracking and
nothing guiding. It then runs the guider's measurement over the frames
with [`guide-replay`](../../crates/guide-replay.md).

This hour, from rig2's QHY5III678M, **is within the tolerance**:
- all 8 guide stars were usable in all 1,441 frames;
- no frame was star-lost;
- each star's scatter was at most 0.11 px, where the limit is 0.5 px.

This is the fourth kind of record [the index](../README.md) names: a
measurement a plan's stage asks for on a rig. Its evidence is the
recorder as it ran, its output frame by frame, and the replay's.

## What was tested

| | |
|---|---|
| Replay | `guide-replay` and `rp-guiding` at [`9ba3f51b`](https://github.com/rusty-photon/rusty-photon/commit/9ba3f51b) (`main`), with the guider's star defaults and stage G3's tolerance |
| Recorder | [`record.ps1`](../../../crates/guide-replay/record.ps1) as of that commit, run on the rig as a one-shot `SYSTEM` scheduled task |
| Service | `qhy-camera` from the nightly suite MSI `0.1.0+nightly.202611071139.g21bfe94`, installed on 2026-11-07, as in the [camera's record](../2026-11-17-qhy-camera-qhy5iii678m-windows/README.md). It ran on its production endpoint, `https://127.0.0.1:11121`, with TLS and the observatory credential, which the recorder read from `rp.json` |
| Rig | rig2: Windows 11 Pro 25H2 x64. A 530 mm f/5 refractor (106 mm aperture) with the QHY600M, its wheel, Optec's FocusLynx and the DSD FP2 cover. The QHY5III678M sits on an off-axis guider and focuses with the FocusLynx |
| Camera | QHY5III678M `QHY5III678M-4a94be45b27d3aa79`, binned 2 × 2 to 1928 × 1090. That is 4.0 µm pixels at 530 mm: 1.557″ a pixel. Gain 30, offset 30, as the camera had them, with 2 s exposures |
| Mount | The rig's Astro-Physics mount, pointed by the owner and tracking at the sidereal rate. Nothing guided it, and it was not touched during the hour |
| Field | RA 21h 40m, Dec +40° (J2000): 1.4 h west of the meridian and 71° up at the start, 2.4 h west and 60° up at the end |
| Time | 2026-11-23 01:34:38 to 02:34:40 UTC (19:34 to 20:34 at the rig), the sun more than 18° below the horizon throughout |

## How it ran

1. **The camera.** The 678M's hub port, USB 3 of the rig's UPBv2, was
   off. `upbv2-driver` switched it on, and `qhy-camera` was restarted
   to find the camera. A one-minute trial in the day recorded 24 frames
   with none failed, and the replay read them.
2. **The first check.** One minute recorded at the start held no light:
   the cover was closed. The owner opened it, and `dsd-fp2` reported
   it open with the panel light off.
3. **The second check.** One minute recorded with the cover open held 8
   guide stars, each usable in every frame. Their half-flux diameter was
   about 6 px.
4. **Focus.**
   - The FocusLynx's USB (UPBv2 USB 5) and motor supply (12 V output 4)
     were switched on, and the focuser was connected. It reported 1.07
     µm a step.
   - The focuser stepped from 29,230 to 29,830 in steps of 60, each
     position approached from below, with one 2 s frame from the 678M
     at each.
   - Lines fitted to the two sides of the V meet at about 29,437, and
     the focuser went to **29,440** from below. It had been at 29,530.
     Two frames there measured 3.4 px, by the same measure that gives
     7.8 px at 29,530.

   [focus-sweep.py](focus-sweep.py) measured the sweep, and its figures
   are in [focus-sweep.csv](focus-sweep.csv). Its measure is a plain
   half-flux diameter over a 22 px aperture with the background from an
   annulus; the replay measures smaller.
5. **The hour.** `record.ps1 -Minutes 60` recorded 1,441 frames in
   3,601 s, at 2.50 s a frame, and none failed. Each frame is the body
   of `imagearray` as the camera sent it, 4,203,084 bytes. The camera
   was disconnected at the end.
6. **The replay.** The hour, 5.7 GB, was copied to a workstation, and
   `guide-replay replay` ran over it in 6 s.

## The replay

| Rule | The hour | Tolerance |
|---|---|---|
| Guide stars selected | 8, on frame 1 | At least 4 |
| Frames measured, and not star-lost | 1,441 of 1,441 | At least 1,000 not star-lost |
| Lost frames, clouded frames | 0, 0 | No lost frame |
| Each guide star usable | In all 1,441 frames | In at least 90 % of the frames it is there in |
| Each star's scatter, on each axis | 0.048 to 0.108 px (0.07″ to 0.17″) | At most 0.5 px |
| Deviations 2 px or more from their star's mean | 0 of 11,528. The largest is 0.38 px | At most 1 in 1,000 |

| Star | Selected at (px) | Usable | Scatter x / y (px) | Mean deviation x / y (px) | Largest (px) |
|---|---|---|---|---|---|
| 0 | 1032.7, 938.5 | 1441 of 1441 | 0.062 / 0.064 | −0.003 / +0.061 | 0.27 |
| 1 | 1566.8, 932.4 | 1441 of 1441 | 0.072 / 0.099 | −0.035 / +0.217 | 0.29 |
| 2 | 1725.9, 406.6 | 1441 of 1441 | 0.073 / 0.049 | +0.002 / +0.040 | 0.27 |
| 3 | 259.8, 704.7 | 1441 of 1441 | 0.100 / 0.064 | −0.074 / −0.071 | 0.38 |
| 4 | 1648.7, 887.0 | 1441 of 1441 | 0.067 / 0.108 | +0.053 / +0.235 | 0.34 |
| 5 | 272.2, 410.2 | 1441 of 1441 | 0.077 / 0.098 | −0.041 / −0.222 | 0.33 |
| 6 | 818.6, 295.0 | 1441 of 1441 | 0.068 / 0.065 | −0.017 / −0.071 | 0.26 |
| 7 | 1392.0, 163.2 | 1441 of 1441 | 0.063 / 0.048 | +0.098 / −0.056 | 0.21 |

**What else the hour shows:**
- **Drift.** Unguided, the field drifted 46.8 px in x and 10.3 px in y
  over the hour: 73″ and 16″. The measurement followed every star
  through it.
- **Field rotation.** The stars on the frame's right (1 and 4) sat
  0.22 px low on average against the others, and star 5 on its left
  0.22 px high. That is the field turning slowly about its centre, as
  an error in polar alignment turns it. The scatter includes that
  trend.
- **Star size.** Each frame's median half-flux diameter was 2.31 px on
  the first frame and 2.87 px on the last. Over the first third of the
  hour its median was 2.6 px, and over the last third 3.0 px (4.0″ to
  4.6″), as focus followed the falling temperature.
- **Signal.** Each frame's median signal-to-noise ratio had a median of
  277 over the hour, and was never below 239. The faintest reading of
  any guide star was 155.

## Files

- `recording/`: the recorder's own output, as it wrote it:
  - [status.txt](recording/status.txt), its log;
  - [camera.json](recording/camera.json), the camera's properties;
  - [frames.csv](recording/frames.csv), a row for each frame.
- `replay/`:
  - [report.json](replay/report.json), the replay's report;
  - [frames.csv](replay/frames.csv), a row for each frame, which
    [summarize.py](replay/summarize.py) derives from the replay's
    per-star `replay.csv` (3 MB, kept with the hour). Each row gives the
    usable stars and whether the frame is star-lost, the frame's offset,
    the median half-flux diameter and signal-to-noise ratio of its usable
    stars, the lowest signal-to-noise ratio among them, and the largest
    deviation of any of them from the frame's offset on either axis.
    That deviation is not taken from each star's mean, as the report's
    is, so it reaches 0.51 px where the report's largest is 0.38 px.
- [focus-sweep.py](focus-sweep.py) and [focus-sweep.csv](focus-sweep.csv):
  the focuser positions and the median half-flux diameter at each.

The hour itself stays out of the repository: 1,441 frames, 5.7 GB.
Twenty-one frames cropped from it are replayed in CI, in
`crates/guide-replay/tests/fixtures/rig2-2026-11-22/`:
- one every 72 frames;
- each 400 × 300 px from (1100, 200);
- cut by `guide-replay crop`.

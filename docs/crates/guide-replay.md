# `guide-replay` Crate Design

Guide-camera frames recorded on a rig, replayed through the guider's
measurement. A recording is an hour of frames from a guide camera, with
the mount tracking and nothing guiding. The replay selects guide stars
as the guider does and measures every frame after it. It then reports
whether the measurement kept its stars, and whether the stars agreed
with one another frame by frame. Nothing gives a recorded frame's true
offset, so the stars' agreement is the check, and not an offset
([guider.md § Testing](../services/guider.md#testing); stage G3 of
[the native guider plan](../plans/native-guider.md#staging)).

This is test infrastructure: `testonly` in Bazel, a library and the
`guide-replay` command. It depends on `rp-guiding`, `ndarray`, `csv`,
`serde`, `serde_json`, `clap` and `thiserror`. The recorder beside it,
`record.ps1`, runs on the rig.

## Scope

- **The recorder:** `record.ps1`, which records a guide camera on a
  Windows rig through its Alpaca driver.
- **The recording:** the directory the recorder writes.
- **Decoding a frame:** the `imagebytes` body a camera sends.
- **The replay:** star selection and measurement over the recording, a
  row for each guide star in each frame.
- **The report:** what the replay found, held to a tolerance.

Out of scope:

- **The control.** Nothing guides while the recorder runs, so there is
  no pulse to replay. The control's closed-loop runs are
  `rp-guiding`'s, against `guide-simulator`.
- **A recorder for a Linux rig.** Stage G3 records on rig2, which runs
  Windows and has no Python.
- **The true offset.** A recording has no truth. `guide-simulator`'s
  frames carry theirs.

## The recorder

`record.ps1 -Out <dir>` records one camera on a `qhy-camera` endpoint,
`https://127.0.0.1:11121` unless `-Base` says otherwise. It records back
to back for `-Minutes` (60), in `-Exposure` second exposures (2), at
`-Bin` × `-Bin` binning (2) over the full sensor.

- **The camera is found by its unique ID** (`-UniqueId`, rig2's
  QHY5III678M unless given) among the endpoint's configured devices.
  Alpaca device numbers follow the SDK's scan order, so a device number
  could name another camera.
- **It touches that camera alone.** Its gain, offset and readout mode
  stay as they are. Nothing it does reaches the mount or another camera.
- **The credential is the one `rp` holds.** The recorder reads it from
  `rp.json`'s camera entry for the endpoint, and never writes it
  anywhere. It trusts only a server certificate that chains to
  rusty-photon's CA.
- **Each frame is kept as the server sent it.** The recorder converts
  nothing, so it keeps pace with 2 s exposures without work per pixel,
  and a frame is what the guider's own client receives. It checks each
  body's header and length before writing it.
- **It stops on its own.** A file named `STOP` in `-Out` ends a
  recording early. At the end, the frame goes back to bin 1 over the
  full sensor and the camera is disconnected.
- **It refuses a directory that already holds a recording**, so that an
  hour is never written over.

It runs on the rig as a one-shot scheduled task, so the recording does
not depend on an ssh session.

## The recording

| File | What it holds |
|---|---|
| `camera.json` | The camera's properties, read before the first frame: `sensortype` and `maxadu` among them, as Alpaca gives them |
| `frames.csv` | One row a frame, failed frames included: `frame`, `file`, `start_utc`, `exposure_s`, `bin`, `numx`, `numy`, `bytes`, `download_ms`, `error` |
| `frame-NNNNN.imagebytes` | The body of `imagearray` asked for as `application/imagebytes`, byte for byte |
| `status.txt` | The recorder's log: its start, a line a minute, each failure, its end |

- **`start_utc`** is when the frame's exposure was asked for, in UTC.
- **A failed frame** has a row with its error and no file.
- **The sensor:** `sensortype` 0 is read as monochrome and 2 as an RGGB
  mosaic. The guider reads no other kind, and a recording of one is
  refused.

## Decoding a frame

An `imagebytes` body is a 44-byte header followed by the pixels. The
header is eleven little-endian 32-bit integers: the metadata version,
the error number, the client and server transaction IDs, where the
pixels start, the image and transmission element types, the rank, and
three dimensions.

- **The pixels are width-major**, as Alpaca's arrays are. The first
  dimension is the width, and `y` runs fastest. The decoded frame is
  indexed `[[y, x]]`, as the guider's frames are.
- **The transmission types are those Alpaca defines for pixels:** 16-bit
  (1), 32-bit (2), 8-bit unsigned (6) and 16-bit unsigned (8), all
  little-endian.
- **A guide frame has one plane.** A rank other than 2, a dimension that
  is not positive, an error number other than 0, a version other than 1,
  pixels that start inside the header, or a body shorter than its frame
  is refused, with the reason.

## The replay

- **Selection** is `rp-guiding`'s, with the guider's star defaults, on
  the first frame that has enough stars to select. The frames before it
  are counted and not measured.
- **Every frame after it is measured** as the guider measures a frame:
  each guide star where it was last usable.
- **A failed frame** is counted and skipped.
- **A star-lost frame is clouded or lost:**
  - It is *clouded* when the measurement saw its stars dimmed: each
    guide star that is not usable is faint, or found with less flux than
    its reference.
  - It is also clouded when a fresh selection finds no stars in it
    either: thick cloud hides them.
  - Otherwise it is *lost*: the stars are there, and the measurement did
    not hold them.
- **A star's deviation** in a frame that is not star-lost is its offset
  less the frame's offset, the median of the usable stars' offsets.
- **`replay.csv`** has a row for each guide star in each measured frame:
  - the star's reading, position, offset and deviation;
  - its signal-to-noise ratio, half-flux diameter and flux, where the
    reading has them;
  - the frame's offset, its usable stars, whether it is star-lost, and
    whether it is clouded.

## The report

The report counts the frames: listed, failed, before the selection,
measured, not star-lost, clouded and lost. For each guide star it gives:

- where it was selected;
- in how many of the frames that are not star-lost it is usable, not
  found, faint or dimmed;
- the mean of its deviation;
- the scatter of its deviation about that mean;
- the largest distance of a deviation from that mean.

**The mean is set aside.** Each offset is measured from where the star
sat in the selection's frame, and that frame's seeing put each star a
little off. That error stays the same for the rest of the recording, so
it shows in the mean and not in the scatter. The scatter is what moves
frame by frame: seeing, noise, and a measurement that wanders.

**The tolerance**, stage G3's pass mark, is in pixels of the measured
frame. The replay passes when all of these hold:

| Rule | Limit |
|---|---|
| Lost frames | None |
| Frames that are not star-lost | At least 1,000 |
| Each guide star usable | In at least 90 % of the frames that are not star-lost |
| Each guide star's scatter, on each axis | At most 0.5 px |
| Deviations 2 px or more from their star's mean | At most 1 in 1,000 |

**Why these limits.** On rig2 a pixel binned 2 × 2 is 1.557″: 4.0 µm at
530 mm.

- **Seeing sets the scatter, not centroid noise.** In replays of
  `guide-simulator`'s frames at that scale in 2.5″ seeing, centroid
  noise adds at most 0.06 px to a star's scatter, down to a
  signal-to-noise ratio of 15. A star's own image motion of 0.3″, 0.5″
  and 0.7″ an axis gives scatters of 0.17–0.22, 0.29–0.37 and
  0.41–0.52 px.
- **0.5 px allows a star's own motion up to about 0.7″.** When PHD2
  guided on rig2, its star moved by a median of 0.50″ RMS in RA and
  0.42″ in Dec, over 55 runs of 2.5 s frames
  ([the plan, § Where the rigs stand](../plans/native-guider.md#where-the-rigs-stand)).
  That figure is the field and the star together, so a star's own
  motion on a median night is less. The limit also leaves room for the
  field rotation an hour of polar error adds.
- **2 px is an outlier.** It is four standard deviations of a star that
  scatters by 0.5 px, where seeing alone puts about one deviation in
  3,000. A star the measurement takes for a hot pixel or a neighbour
  puts it there frame after frame.
- **1,000 frames that are not star-lost** are about 40 minutes at
  rig2's 2.5 s cadence.

A tolerance with a different count of frames serves a shorter
recording, such as the cropped frames the tests replay.

## The command

`guide-replay <recording> [--out <dir>]`:

- replays the recording with the guider's star defaults and stage G3's
  tolerance;
- writes `replay.csv` and `report.json` into `--out`, or into the
  recording when `--out` is not given;
- prints the report.

It exits 0 when the replay is within the tolerance, 1 when it is not,
and 2 when the recording cannot be read or measured.

## Testing

The unit tests write recordings the way the recorder does:
- bodies and `frames.csv` rows written by hand;
- frames that `guide-simulator` renders at rig2's binned scale, with
  drift and seeing.

They replay them and hold the replay to what each case shows:
- a tracked field that keeps its stars, within the tolerance;
- frames before any star, and failed frames;
- a cloud whose frames are clouded;
- stars the measurement loses to a jump of the field, and stars that
  all brighten past the flux test, which are lost;
- a star whose light is moved off its place, for half the recording or
  for two frames, which scatters or makes outliers;
- a star's scatter and outliers, measured about its own mean.

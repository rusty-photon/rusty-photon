# Guide-log fixtures

Guide logs that PHD2 wrote on the rigs, whole and byte for byte. They are the
record of the guide-log format the guider's own log is held to: the guider's
writer is tested against what a parser built from these files accepts
(`docs/services/guider.md`, § Guide log). PHD2's description of the format is
<https://github.com/OpenPHDGuiding/phd2/wiki/PHD2GuideLog>.

Everything this file says about a log is a count of that log's lines or a
quotation from it. Nothing here says what PHD2 does in a case the files do not
hold.

| File | Written by | Bytes | Lines | SHA-256 |
|---|---|---|---|---|
| `PHD2_GuideLog_2025-05-07_203810.txt` | rig2, PHD2 2.6.13 [Windows] | 432 | 10 | `e86a94769d862a2f12b8b9c223aa42022cff1f747d74f02ae7ac475e9624a50a` |
| `PHD2_GuideLog_2025-05-13_002522.txt` | rig2, PHD2 2.6.13 [Windows] | 67,356 | 810 | `19044366b4f829466bbd1a71aef61ceed0b3cea89a5dba47f0f7ff710050f35d` |
| `PHD2_GuideLog_2025-05-21_210354.txt` | rig2, PHD2 2.6.13 [Windows] | 668,970 | 7,917 | `4a6a65ca22063cc66b64663d40d9b46b0cf63159fef1bd5b8f5de57bd0bdeba8` |
| `PHD2_GuideLog_2025-06-17_231857.txt` | rig2, PHD2 2.6.13 [Windows] | 415,906 | 5,337 | `567a6554c40e8348d706a939375e15f01f20ee91b0296e5204bfa3d05b2c8dbe` |
| `PHD2_GuideLog_2026-10-13_143433.txt` | pier1, PHD2 2.6.14 [Linux] | 1,968 | 40 | `d5e55cf652a0d076ca7ca3a078c704025aaef70d1dbd3418e39b0dfe101e52f3` |

rig2's logs end every line with CR LF and pier1's with LF, and
`.gitattributes` keeps the files out of line-ending conversion, so a checkout
on any platform holds the bytes PHD2 wrote. Every file is ASCII, calls itself
`Log version 2.5` and ends with a line ending.

## The two rigs

- **rig2**'s logs are of real stars and a real mount, in May and June 2025.
  The PHD2 profile in them is `QHY678M`: a QHY5III678M guide camera at
  2.0 µm pixels and 530 mm (0.78″ per pixel), 3856 × 2180 at bin 1, and an
  Astro-Physics mount through its ASCOM driver, which the logs call
  `AstroPhysicsV2 (ASCOM)`. Guide speed is 15.0″/s on both axes. RA runs the
  `Hysteresis` algorithm and Dec `Resist Switch`, in multi-star mode.
- **pier1**'s log is of PHD2's simulator. The PHD2 profile in it is
  `Simulator`: the camera is `Simulator` and the mount `On Camera`, and the
  header gives the guide speeds, the pointing and the pier side as
  `Unknown`.

## What a log is made of

A file holds one or more **blocks**. A block opens with
`PHD2 version …, Log version 2.5. Log enabled at <time>` and closes with a
blank line, `Log Summary: calcnt:N gcnt:N gdur:N gacnt:N` and
`Log closed at <time>`. The blocks of one file all carry the same
`Log enabled` time, and the summary's counts run on from one block of a file
to the next: in every fixture `calcnt` is the number of calibrations completed
so far in the file and `gcnt` the number of `Guiding Ends` lines so far.

Inside a block are **sections**:

- **A calibration** opens with `Calibration Begins at <time>`, ten header
  lines and the column heading `Direction,Step,dx,dy,x,y,Dist`. Its step rows
  begin `West,`, `East,`, `Backlash,`, `North,` or `South,` and have seven
  fields. `West calibration complete. Angle = … deg, Rate = … px/sec,
  Parity = Even` follows the West rows, the same line for `North` follows the
  North rows (with `Parity = Odd` or `Even`), and
  `Calibration complete, mount = ….` closes a calibration that ran to its
  end. Three calibrations in these files are given up, two in their West rows
  and one before its first row: none has a completion line, and a
  `Guiding Ends at <time>` line follows each instead.
- **A guiding run** opens with `Guiding Begins at <time>`, fifteen header
  lines and the column heading below, and closes with `Guiding Ends at
  <time>`.

  ```text
  Frame,Time,mount,dx,dy,RARawDistance,DECRawDistance,RAGuideDistance,DECGuideDistance,RADuration,RADirection,DECDuration,DECDirection,XStep,YStep,StarMass,SNR,ErrorCode
  ```

A **guide-step row** takes one of two forms:

- A `"Mount"` row has the heading's 18 fields, with `ErrorCode` 0. `XStep`
  and `YStep` are always empty. `RADirection` is `E` or `W` and
  `DECDirection` `N` or `S`; each is empty exactly when its duration is `0`,
  which gives the row four shapes.
- A `"DROP"` row has 19 fields: frame, time and `"DROP"`, twelve empty
  fields, then star mass, SNR, the error code and a nineteenth field with the
  reason in quotes. Five codes occur: 2 `"Star lost - low SNR"`,
  3 `"Star lost - low mass"`, 4 `"Star lost - low HFD"`, 5 `"No star found"`
  and 8 `"No star found"`. Rows of code 8 carry a star mass and an SNR; rows
  of the other codes carry `0` and `0.00`.

`Time` starts over in each run: every run's first row is frame 1 with a
`Time` between 1.379 and 5.103, and `Time` never falls from one row of a run
to the next. The hours given for a log below read `Time` as seconds and add
up the `Time` of each run's last row.

**`INFO:` lines** stand between rows and between sections:
`DITHER by …, new lock pos = …`, `SET LOCK POSITION, new lock pos = …`,
`SETTLING STATE CHANGE, Settling started` (and `complete`, `failed`),
`Guiding parameter change, MultiStar = true` (and `Exposure = N ms`), and
`STAR LOST during calibration, Mass= …, SNR= …, Error= …, Status=…`.

Five things in the files that a parser has to accept:

- A block closes while a guiding run is open. The run that begins at
  2025-05-21 22:19:25 has no `Guiding Ends` line: its rows are followed by
  the block's `Log Summary`.
- `Guiding Ends` lines outnumber `Guiding Begins` lines. One follows each
  calibration that is given up, and in the 2025-05-21 log two follow each
  other with one `INFO:` line between them (22:11:00 and 22:11:33).
- The camera line of a header begins with a space.
- A settle outcome stands with no `Settling started` before it. In the
  2025-05-21 log a `Settling failed` note follows the `Guiding Ends` line
  of the calibration given up at 21:54:16, and pier1's log has one
  `Settling failed` and no `Settling started`. So that 2025-05-21 log has
  15 settles started against 12 complete and 4 failed.
- The exposure changes inside a run. In the 2025-05-21 log five
  `Guiding parameter change, Exposure = N ms` notes stand between the rows
  of two runs, so a run's header does not give the exposure of all its rows.

## `PHD2_GuideLog_2025-05-07_203810.txt`

The smallest log with more than one block, and with CR LF line endings. It
holds no calibration and no guiding: two blocks of a `Log enabled` line, a
`Guiding parameter change, MultiStar = true` note, a summary of zeros and a
`Log closed` line. Both blocks carry `Log enabled at 2025-05-07 20:38:10`;
the first closes at 2025-05-07 20:38:20 and the second at
2025-05-09 20:36:15.

## `PHD2_GuideLog_2025-05-13_002522.txt`

One short night with nothing going wrong.

- **Calibration:** one, run to its end, with 21 West, 3 East, 8 Backlash,
  13 North and 4 South step rows. West: angle 10.5°, 10.932 px/s, parity
  even. North: angle 103.5°, 18.225 px/s, parity odd.
- **Guiding:** 2 runs, 690 guide-step rows and no `DROP` row, 0.60 h, at
  2000 ms exposures, Dec 54.2°, pier side East, parity `+/-`.
- **Dither and settle:** 2 dithers; 4 settles started, 3 complete and
  1 failed.
- The guiding headers carry `Last Cal Issue = None` with a timestamp that
  ends `AM`.

## `PHD2_GuideLog_2025-05-21_210354.txt`

A whole night with a troubled start, in two blocks.

- **Calibration:** 4 begun and 2 run to their end, with 25
  `STAR LOST during calibration` notes of both statuses (`low HFD` 24,
  `low SNR` 1). Both completed calibrations have West angles near −168° and
  North parity even.
- **Guiding:** 19 `Guiding Begins` and 21 `Guiding Ends` lines, 7,296
  guide-step rows, 7.10 h, on both pier sides, at parities `+/+` and `+/-`,
  Dec 30.9° to 54.3°.
- **Dropped frames:** 81 `DROP` rows: code 2, 73; code 3, 7; code 8, 1.
- **Exposure:** the run headers carry `Exposure = 500 ms` (7 runs, 162
  rows) and `Exposure = 2500 ms` (12 runs, 7,134 rows). Five
  `Guiding parameter change, Exposure = N ms` notes stand between the rows
  of two runs and name 1000, 200, 2500, 3000 and 2500 ms, in that order.
  Counting each row under the last exposure written before it in its run,
  by the header or by a note, gives 500 ms 109 rows, 200 ms 35,
  1000 ms 11, 2500 ms 7,133 and 3000 ms 8.
- **Dither and settle:** 2 dithers; 15 settles started, 12 complete and
  4 failed.

## `PHD2_GuideLog_2025-06-17_231857.txt`

The last night PHD2 guided on rig2, and the log with the most dropped frames:
a third of its rows are `DROP` rows.

- **Calibration:** one, run to its end, with 20 West, 3 East, 4 Backlash,
  16 North and 4 South step rows. West: angle 9.3°, 11.331 px/s, parity
  even. North: angle 109.7°, 14.423 px/s, parity odd. The guiding headers
  after it carry `Last Cal Issue = Rates` and
  `ortho.err. = 10.4 deg`.
- **Guiding:** 29 runs, 4,584 guide-step rows, 4.92 h, at 2500 ms
  exposures, on both pier sides, Dec 30.9° to 54.3°.
- **Dropped frames:** 1,526 `DROP` rows, of all five codes: code 2, 830;
  code 4, 590; code 8, 84; code 5, 19; code 3, 3.
- **Dither and settle:** 19 dithers; 48 settles started, 9 complete and
  39 failed.

## `PHD2_GuideLog_2026-10-13_143433.txt`

The one log PHD2 on Linux wrote with a section in it, and the one with LF line
endings. It has five blocks, all carrying
`Log enabled at 2026-10-13 14:34:33`. The second holds a calibration that
never took a step: its header, the column heading, then
`INFO: SETTLING STATE CHANGE, Settling failed` and
`Guiding Ends at 2026-10-13 15:20:29`. Its header has the lines no rig2 log
has:

```text
Equipment Profile = Simulator
Search region = 15 px, Star mass tolerance = 50.0%, Multi-star mode, list size = 1
 Camera = Simulator, gain = 95, full size = 752 x 580, no dark, no defect map, pixel size = 4.6 um
Mount = On Camera, Calibration Step = 1650 ms, Calibration Distance = 25 px, Assume orthogonal axes = no
RA Guide Speed = Unknown, Dec Guide Speed = Unknown
RA/Dec = Unknown, Hour angle = Unknown, Pier side = Unknown, Rotator pos = N/A, Alt = Unknown, Az = Unknown
```

## What the set leaves out

The rigs held 17 guide logs on 2026-11-13: 14 on rig2 (5.8 MB, 2025-05-04 to
2025-06-17) and 3 on pier1 (2026-10-13 to 2026-10-29). Counting two lines as
one kind when they differ only in their numbers, a number taken with its
sign, the 17 logs hold 69 kinds of line, the empty line among them, and these
five files hold 68.

The one left out is the camera line of a rig2 night guided without a dark
library, which only `PHD2_GuideLog_2025-05-11_204955.txt` (632 kB) has:

```text
 Camera = QHY5III678M-4a94be45b27d3aa79, gain = 40, full size = 3856 x 2180, no dark, no defect map, pixel size = 2.0 um
```

Its `no dark` clause is in pier1's file, with the simulator camera.

No log on either rig holds a guiding run on pier1's mount, a defect map, a
rotator position, backlash compensation switched on, or a guide algorithm
other than `Hysteresis` on RA and `Resist Switch` on Dec.

No line in any of the 17 logs holds the word `pause` or `resume`, in capitals
or not, so the files give no form for a note of a pause or of a resume. A
frame with the star lost while guiding is a `"DROP"` row in them, and the
only `INFO:` lines about a lost star are the `STAR LOST during calibration`
ones.

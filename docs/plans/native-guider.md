# Native guider — options for guiding without PHD2

**Date:** 2026-11-12
**Parent design docs:** [`docs/workspace.md`](../workspace.md#project-tenets) (project tenets), [`docs/services/guider.md`](../services/guider.md), [ADR-024](../decisions/024-native-guider-as-a-standalone-alpaca-client-service.md), [`docs/services/rp.md`](../services/rp.md), [`docs/services/phd2-guider.md`](https://github.com/rusty-photon/rusty-photon/blob/865fe440e74ebc305269774bac4d9b9ac9da44f1/docs/services/phd2-guider.md), [`docs/plans/optical-trains.md`](optical-trains.md), [`docs/plans/daily-power-cycle.md`](daily-power-cycle.md)

> **Status:** decided on 2026-11-12. Stages G0, G6, G1, G2, G3 and G4
> are merged, stages G5 and G7a are in progress, and G7 has not started. The owner read this study and settled its
> main questions: rusty-photon builds a native guider as a standalone
> service that is an Alpaca client, and drops PHD2
> ([ADR-024](../decisions/024-native-guider-as-a-standalone-alpaca-client-service.md)).
> The service is designed in [`guider.md`](../services/guider.md).
> § Staging orders the work, and § Decisions for the owner records each
> decision as the owner settled it; none is open. The options and the
> evidence below are the study as the owner read it, with the
> corrections a review of it found, with what G0 read from the rigs on
> 2026-11-13 in § Where the rigs stand, and with what the owner settled
> on 2026-11-14: the acceptance in § 9, and that PHD2 leaves the tree
> before the guider is built (§ Staging). What the study quotes from
> `rp.md` is that document as it stood on 2026-11-12; stage G6 has
> rewritten its passages about the guider.

## Goal

rusty-photon guides through PHD2, wrapped by
[`phd2-guider`](https://github.com/rusty-photon/rusty-photon/blob/865fe440e74ebc305269774bac4d9b9ac9da44f1/docs/services/phd2-guider.md). PHD2 is a desktop
application, and most of what the wrapper, the packaging and the
[daily power cycle plan](daily-power-cycle.md) carry for guiding exists
to make a desktop application behave like an unattended service. This
study asks whether rusty-photon should guide with a service of its own
instead, what shapes that service can take, how each fits the Alpaca
ecosystem and rusty-photon's own architecture, and what it costs.

The recommendation in brief, and what the owner decided on 2026-11-12:

1. **Build a native guider**: a pure-Rust core library (frames in,
   corrections out) hosted by a headless service. PHD2 stays selectable
   beside it until the native guider has guided real nights on both
   rigs. **Decided, and further than recommended:** the native guider
   is built and PHD2 is dropped, not kept selectable. If the native
   guider fails, PHD2 support comes back from the repository's history.
2. **The guider is an Alpaca application, not an Alpaca device.** It is
   a client of one Alpaca Camera and one Alpaca Telescope. Alpaca has no
   guider device type, and none of the software surveyed here exposes a
   guiding process as an Alpaca device. **Decided.**
3. **It is a standalone service, not a part of `rp`.** It serves the
   HTTP contract `rp` already consumes from `phd2-guider`, with the PHD2
   vocabulary taken out. `rp`'s design names that seam: "Swapping in a
   different guiding backend requires only a different guider service
   that implements the same HTTP endpoints"
   ([rp.md § Guider Service](../services/rp.md#guider-service)).
   **Decided:** the owner weighed the tool-provider placement (L3) and
   settled on the standalone service. The design changes the contract
   by more than its vocabulary (§ Recommended shape).
4. **Start with the loop every working guider runs**: multi-star
   centroids with outlier rejection, a step calibration, a proportional
   response with hysteresis, a minimum move and a pulse cap. Predictive
   and assisted pieces come later, each against a measurement.
5. **Write PHD2-format guide logs from the first version**, so the
   existing log viewers read them. A PHD2-compatible event server is a
   later, optional surface. **Decided** for the guide log; the event
   server is not planned (§ Decisions for the owner, 5).
6. **Build the simulator before going on sky**, so that nights confirm
   behaviour instead of debugging it (tenet 1).
7. **Hold the Windows session keeper** (decision 10's Windows half)
   until this is decided. It is the largest piece of unbuilt work that
   exists only for PHD2. **Decided: held**, and with PHD2 dropped it is
   not built.

What this study could not establish is in § Not verified. The two gaps
that matter most: no sustained guide-cadence measurement exists through
any camera driver, and no on-sky guiding under `rp` is on record with
any guider. Stage G1 measured the first on 2026-11-17 (§ Staging, "What
G1 found").

## Background: what guiding through PHD2 costs

### Four facts about PHD2

Each cost below follows from one of four properties of PHD2. None is a
defect in PHD2: it is built to be set up and watched by a person at a
screen.

1. **It is a desktop application with no headless mode.** It always
   shows its main window, and a new profile opens its wizard. The
   upstream request for a server API and headless setup has been open
   since 2018
   (<https://github.com/OpenPHDGuiding/phd2/issues/683>); the
   maintainers' answers there and on the project's forum are "We
   primarily envision PHD2 as something that a user sets up
   interactively", "PHD2 currently _cannot_ run without a GUI" and, in
   January 2025, "There is no "headless" option for PHD2"
   (<https://groups.google.com/g/open-phd-guiding/c/KDD-TVvkZ-4>,
   <https://groups.google.com/g/open-phd-guiding/c/AcxQ4Fn00Ws>). The
   project's README says it no longer accepts pull requests from all
   GitHub users. One fork aims at a headless, Alpaca-only daemon
   (<https://github.com/open-astro/openastro-guider>, created
   2026-06-02); its README says that "the binary is still the windowed
   PHD2 application".
2. **It owns its devices through its own drivers.** On the rigs PHD2
   opens the guide camera through the vendor SDK built into it, so the
   camera cannot also be served by its rusty-photon driver. The stable
   release (2.6.14) reaches a mount on Linux through INDI or an ST4
   port only. A native Alpaca camera and mount client was merged into
   PHD2's development branch on 2026-07-24
   (<https://github.com/OpenPHDGuiding/phd2/pull/1454>) and is in no
   stable release at this study's date.
3. **It is configured through its GUI.** Equipment is chosen in a
   profile wizard, dark libraries and defect maps are built from menus,
   and the profile is written to disk on a clean exit, not while PHD2
   runs.
4. **Its control surface is built for an interactive application.** The
   event server (JSON-RPC over TCP port 4400) accepts any local client
   with no authentication; it has no dither abort; it reports the pixel
   scale only after a calibration and the star's half-flux diameter
   only while guiding; its rotator slot is read-only; and a full pause
   sent during a frame leaves PHD2 refusing every later capture until
   it is restarted
   ([phd2-guider.md § Camera facade](https://github.com/rusty-photon/rusty-photon/blob/865fe440e74ebc305269774bac4d9b9ac9da44f1/docs/services/phd2-guider.md#camera-facade-camera)).

### What rusty-photon carries because of them

Line counts are production lines on `main` at this study's date, with
in-file test modules excluded.

| What the stack carries | The PHD2 fact behind it | State |
|---|---|---|
| The PHD2 client in `phd2-guider`: JSON-RPC, event parsing, process start and stop, reconnect. 3,128 lines in files that hold nothing else | PHD2 speaks its own protocol and is a separate program | Built |
| The [Camera facade](https://github.com/rusty-photon/rusty-photon/blob/865fe440e74ebc305269774bac4d9b9ac9da44f1/docs/services/phd2-guider.md#camera-facade-camera): an Alpaca Camera that takes single frames through PHD2, with a capture slot and a sweep lease. 2,486 lines in its three files, 46 of the service's 85 BDD scenarios, a second port bound to loopback only | PHD2 holds the guide camera, so a focus sweep cannot use the vendor driver | Built. No gain, offset, binning or pulse guiding through it; it works only while PHD2 is stopped |
| A mock PHD2 (939 lines) and 4,670 lines of integration tests against it | CI cannot run a desktop application | Built. The mock is the only PHD2 the tests see; its `save_image` reply once had a shape real PHD2 never sends, and the client that matched it passed CI |
| Two ways to focus the guide camera: `rp`'s guide-train `auto_focus`, which reads HFD from PHD2's guide-step events and so needs an active guide loop, and focus-model's `focus_train` through the facade, which needs PHD2 stopped | HFD only while guiding; the camera is PHD2's | Built. Neither is validated on sky |
| A desktop session with nobody logged on: on Linux a virtual display unit; on Windows a dedicated account whose session `phd2-guider` keeps alive through an RDP connection to the host itself; on macOS no unattended path ([decision 10](daily-power-cycle.md#decisions)) | No headless mode | Linux half in review at this study's date; Windows half designed, not built |
| A restart route (`POST /api/v1/phd2/restart`, `rp`'s `restart_guider`) and a restart at every dusk | PHD2's QHY support does not look for cameras again after finding none; the full-pause wedge | Same as the row above |
| PHD2 built from source on each Debian or Raspberry Pi OS host | Neither distribution packages it | Per rig, by hand |
| A profile made by hand in the wizard on each rig, at risk until PHD2 next exits cleanly | GUI configuration | Per rig, by hand |
| The guide camera kept out of its vendor driver's device list | SDK ownership | Config on rig2. pier1's guide camera is under `qhy-camera` |
| A protocol that is "the one exception to the Alpaca-only rule" in `rp`'s design | Its own protocol | In [rp.md § Guider Service](../services/rp.md#guider-service) |
| A guider the train model cannot see into: "a global service block with no camera association"; the `guide_rotator_unmodeled` event; a backlog item for an INDI-to-Alpaca rotator bridge so that PHD2 on Linux can read a rotator ([optical-trains.md](optical-trains.md)) | PHD2 owns its equipment list | Built, with the bridge unbuilt |
| Guiding results in guide-camera pixels; a dither that cannot be aborted | Pixel scale only after calibration; no dither abort | In the HTTP contract |

Counted by whole file, `phd2-guider`'s 9,125 production lines are 34 %
PHD2 client, 27 % facade and 38 % files any guider service needs (the
HTTP API, RMS statistics, config, the setup page). Those shared files
also hold PHD2- and facade-specific code; apportioned by what each part
serves, the split is nearer 40 / 34 / 25 (an estimate).

### What PHD2 gives

- **Field-proven behaviour.** More than a decade of use on every common
  mount. Its calibration sanity checks, its star-selection rules and
  its handling of Dec backlash are the accumulated answers to failures
  seen in the field.
- **Defaults that work.** A new user on an unknown mount usually guides
  on the first night.
- **Multi-star guiding, predictive PEC, the Guiding Assistant** (which
  measures polar alignment error, backlash and seeing and recommends
  settings), dark libraries and defect maps.
- **The guide log and its viewers**, and a community that reads those
  logs: "post your guide log" is how guiding problems get diagnosed.
- **A window to look at.** The star profile and the graph are the
  fastest way for a person to see what is wrong.

A replacement starts without any of these and has to earn each one.

### Where the rigs stand

Stage G0 read both rigs on 2026-11-13 and changed nothing on either:
PHD2's profiles, its debug and guide logs and its binary, and the
rusty-photon configuration beside it. Five of the guide logs are the
fixtures in `services/guider/tests/fixtures/guide-logs/`, and the
README there says what each one holds.

**pier1** is a Raspberry Pi 5 with a Star Adventurer GTi on the
[`star-adventurer-gti`](../services/star-adventurer-gti.md) driver. Its
guide camera, a QHY5III715C (colour), sits on an off-axis guider with a
motorised helical focuser ([light path](optical-trains.md#goal)).

- **Nothing on pier1 shows PHD2 driving a mount.** PHD2 2.6.14 is built
  from source there, and the logs it holds are of three sessions, on
  2026-10-13, 2026-10-14 and 2026-10-29. In them PHD2 connected its
  equipment 13 times: eight times its simulator camera with the
  "On-camera" mount (`Connected Scope:On Camera`), and five times "QHY
  Camera" with no mount (`Connected Scope:None`), three of which found
  the QHY5III715C. The one `guide` request in the debug logs met the
  simulator: the guide log has a calibration header with no step under
  it, then `Settling failed`.
- **As installed, PHD2 reaches the GTi through an ST4 cable or not at
  all.** Its binary holds the mount names "INDI Mount", "On-camera" and
  "Simulator" and no string that names Alpaca, as fact 2 above says of
  the stable release. The host has no INDI server: none is installed as
  a package, none is on the path and none is running. Whether an ST4
  cable runs from the guide camera to the mount is not known.
- **Neither of its two profiles has a mount that is hardware.**
  "Simulator", the current one, pairs the simulator camera with
  "On-camera". "QHY5III715C guide (no mount)" has "QHY Camera" at
  1.45 µm pixels, 360 mm and 1 s exposures, and no mount, aux mount or
  rotator.
- **The guide camera is under `qhy-camera` today.** That driver takes
  every QHY camera it finds on pier1 and serves one. `rp` has the
  camera as `qhy5iii715c` in a `guide` train, with the main focuser and
  the off-axis guider's, and its mount entry points guiding at
  `phd2-guider`, which is installed and running with PHD2 stopped.

**rig2** is an Intel NUC on Windows 11 Pro with an Astro-Physics mount.

- **PHD2 reaches the mount through its ASCOM driver, not through
  Alpaca.** PHD2 2.6.13 has one profile, `QHY678M`. Its mount is
  "AstroPhysics GTO V2 Mount (ASCOM)", which PHD2's debug log resolves
  to the COM driver `AstroPhysicsV2.Telescope`; the driver installed is
  "ASCOM Astro-Physics V2 Telescope Driver v5.60.17". The profile has
  no aux mount, rotator or adaptive optics.
- **The guide camera is PHD2's own.** The profile's camera is "QHY
  Camera", which is PHD2 calling the QHY SDK itself (its debug log has
  `QHY: call SetQHYCCDResolution roi = 3856,2180`), on a QHY5III678M
  with 2.0 µm pixels at 530 mm: 0.78″ per pixel. `qhy-camera` on rig2
  lists the imaging camera alone in `usb_devices`, which leaves the
  QHY5III678M to PHD2.
- **The profile is set up and calibrated.** PHD2 states its settings
  in the header of every guiding run. These lines are from the first
  run of the last night, 2025-06-17:

  ```text
  Pixel scale = 0.78 arc-sec/px, Binning = 1, Focal length = 530 mm
  Search region = 15 px, Star mass tolerance disabled, Multi-star mode, list size = 12
   Camera = QHY5III678M-4a94be45b27d3aa79, gain = 40, full size = 3856 x 2180, have dark, dark dur = 2500, no defect map, pixel size = 2.0 um
  Exposure = 2500 ms
  Mount = AstroPhysicsV2 (ASCOM), connected, guiding enabled, xAngle = 9.3, xRate = 11.331, yAngle = 109.7, yRate = 14.423, parity = +/-
  Norm rates RA = 15.1"/s @ dec 0, Dec = 11.2"/s; ortho.err. = 10.4 deg
  X guide algorithm = Hysteresis, Hysteresis = 0.100, Aggression = 0.700, Minimum move = 0.350
  Y guide algorithm = Resist Switch, Minimum move = 0.350 Aggression = 100% FastSwitch = enabled
  Backlash comp = disabled, pulse = 20 ms
  Max RA duration = 2500, Max DEC duration = 2500, DEC guide mode = Auto
  RA Guide Speed = 15.0 a-s/s, Dec Guide Speed = 15.0 a-s/s, Cal Dec = 54.2, Last Cal Issue = Rates, Timestamp = 6/17/2025 11:29:38 PM
  ```

  The calibration those lines name took 125 ms steps over 26 px, and it
  is the one PHD2 still stores. The header carries
  `Last Cal Issue = Rates` and `ortho.err. = 10.4 deg` for it.
- **PHD2 guided there in May and June 2025.** rig2 holds 14 guide logs,
  of 2025-05-04 to 2025-06-17, and three of them hold no guiding. The
  other eleven hold 179 guiding runs and 62.2 h, adding up the `Time`
  of each run's last row: 64,086 guide-step rows, 5,730 of them (8.9 %)
  `DROP` rows, whose reason reads "Star lost" or "No star found"; 17
  calibrations begun and 11 completed; 233 dithers; 396
  `Settling started` notes, 254 `complete` and 148 `failed`, six of
  those outcomes with no `Settling started` before them. Declinations
  run from 30.9° to 54.3°, on both pier sides.
- **How well it guided.** The 55 runs of ten minutes or more hold
  57.0 h, counted the same way. For each, take the RMS of the star's
  raw offset about its mean over the run: `RARawDistance` and
  `DECRawDistance` of the `"Mount"` rows, each axis about its own mean,
  the two combined in quadrature, at 0.78″ per pixel. Across the 55
  runs the median is 0.69″, with quartiles at 0.49″ and 1.43″; RA alone
  has a median of 0.50″ and Dec 0.42″. Dither and settle frames are in
  those figures, so they overstate the error between dithers.
- **A client drove PHD2, and the logs do not name it.** In the last
  night's debug log, `get_connected` and `set_connected` requests on
  PHD2's event server come before the equipment connects.
- **Nothing on rig2 shows PHD2 running since.** Its newest log, debug
  or guide, is of the night of 2025-06-17, and the calibration it
  stores is that night's.
- **rig2 does not guide under `rp`.** `phd2-guider` is not installed
  there. `rp` on rig2 has one train, the imaging train, and no mount
  entry, so it has no guiding entry either.

**No on-sky guiding under `rp` is on record**, on either rig. The
validation records that involve PHD2 are facade runs and one indoor run
of the guiding train with no stars.

So the native guider displaces no guiding that runs under `rp`, since
neither rig has any. What it displaces, at G7, is rig2's PHD2 setup:
calibrated, with 62 h of logged guiding, driven from outside `rp` and
last used in June 2025. The costs in the table above have been paid
ahead of the benefit.

## What a guider does

Sizes are from a reading of PHD2's source: of the 107,750 lines under
`src/`, 41,784 are its device layer, about 27,750 its GUI and 21,540
its core guiding. The "knowledge" column estimates the lines that hold
guiding knowledge once GUI coupling is set aside (±25 %).

| Job | What it involves | Knowledge lines in PHD2 | How hard to do well |
|---|---|---|---|
| Take frames | Loop exposures of 1–4 s; subtract a dark or apply a defect map | (device layer) | Easy with a driver; the cadence has to be measured |
| Find and choose stars | Detect, reject hot pixels, saturated stars and close pairs, rank by SNR | ≈ 1,000 | Detection is easy; never locking on the wrong thing is hard |
| Measure displacement | Sub-pixel centroid per star; combine several stars; notice a lost star | ≈ 1,700 | Centroid easy; the multi-star rules are judgement |
| Calibrate | Find the camera's angle, the pixel rate of each axis, the parity and the Dec backlash by pulsing the mount and watching the star | ≈ 2,400 | **The hardest part**: it runs on an untuned mount, and every later correction depends on it |
| Decide the correction | Per axis: error in, pulse out, with a dead band, a cap and Dec's reluctance to reverse | ≈ 650; predictive PEC ≈ 3,000 more | The simple algorithms are a few lines each; PEC is a research topic |
| Handle Dec backlash | Measure it, compensate or guide in one direction only | ≈ 1,000 | Hard: the mount hides it |
| Send pulses | Issue, wait, do not smear the next frame | ≈ 200 | Small, with driver quirks |
| Dither and settle | Offset the lock position; decide when the star is back | (in the above) | Easy |
| React to events | Slew, park, flip, tracking off, clouds | (in the above) | Moderate; PHD2 does not check tracking or park state before a pulse |
| Report | Per-frame statistics, the guide log, the event stream | ≈ 700 log; ≈ 3,000 event server | Easy, long |
| Help tune | The Guiding Assistant | ≈ 550 plus GUI | Heuristics from experience |

Totals: about 10,000–13,000 lines of knowledge in PHD2. A minimal
equivalent is an estimated 5,000–6,000 lines of C++, or 9,000–10,000
with multi-star, backlash handling and assistant analysis. The guiding
cores of the other open-source guiders run from 3,000 to 7,000 lines
before any Gaussian-process predictor: CCDciel 2,955, INDIGO 3,485 plus
part of a 3,767-line utility file, Ekos about 5,700.

The device layer, PHD2's largest part, is the part rusty-photon already
has: its Alpaca drivers.

## What the Alpaca ecosystem offers a guider

### The inputs and outputs are standard members

| Need | Alpaca member | Note |
|---|---|---|
| A frame | Camera `StartExposure`, `ImageReady`, `ImageArray` (ImageBytes transfer) | One-shot exposures; the standard has no streaming |
| A subframe, binning, gain | Camera `StartX`/`StartY`/`NumX`/`NumY`, `BinX`/`BinY`, `Gain`, `Offset` | One region of interest per exposure |
| Pixel scale | Camera `PixelSizeX`/`PixelSizeY`, with a configured focal length | Known before any calibration |
| A correction | Telescope `PulseGuide`, `IsPulseGuiding`, `CanPulseGuide` | Asynchronous by contract. Pulses on both axes at once are optional for a mount |
| A correction through an ST4 cable | Camera `PulseGuide`, `IsPulseGuiding`, `CanPulseGuide` | For mounts with no usable `PulseGuide` |
| Expected pulse response | Telescope `GuideRateRightAscension`, `GuideRateDeclination` | What PHD2 asks the user or measures |
| Declination, pier side | Telescope `Declination`, `SideOfPier` | Dec compensation and flip handling without asking anyone |
| Whether a pulse is safe | Telescope `Tracking`, `Slewing`, `AtPark` | Read before every pulse |
| Field rotation | Rotator `Position` | Adjusts the calibration angle; PHD2 on Linux cannot read an Alpaca rotator |

A guider that is an Alpaca client gets from the standard what PHD2
needs wizard pages, an "aux mount" connection or a calibration run to
learn.

### What Alpaca does not have

- **A guider device type.** The standard defines ten device types, none
  of them a guider, and no way to register a custom type; `Action` and
  `SupportedActions` are its extension mechanism.
- **Timing guidance.** ASCOM gives no latency budget, minimum pulse or
  polling interval. Its only rule is that methods be asynchronous
  (<https://ascom-standards.org/newdocs/relnotes.html>). `PulseGuide`
  on a parked mount is not specified.
- **Per-client sessions.** `Connected`, the settings and the single
  image slot of a camera are shared by every client. Two clients of one
  camera have to be coordinated by something outside the standard.

### Where guiding sits in every other ecosystem

| Software | How a client controls guiding |
|---|---|
| PHD2 | JSON-RPC event server, TCP 4400 |
| INDIGO guider agent | INDIGO properties on the agent |
| KStars / Ekos | DBus `org.kde.kstars.Ekos.Guide` |
| SkyGuard | HTTP GET requests with a callback |
| N.I.N.A. | The in-process `IGuider` interface; plugins add guiders through `IEquipmentProvider<IGuider>` |
| N.I.N.A. Advanced API (third party) | HTTP `/equipment/guider/*` |

Every one of these offers the same verbs: connect equipment, calibrate
and clear calibration, start, stop, dither with settle parameters, and
a per-frame statistics stream. None presents the guiding process as an
Alpaca device. Where Alpaca appears, the guider is the **client**:
PHD2's development branch drives an Alpaca camera and mount, and
INDIGO's `indigo_system_alpaca` driver does the same for INDIGO.

### The de-facto standards are PHD2's

- **The event-server protocol**
  (<https://github.com/OpenPHDGuiding/phd2/wiki/EventMonitoring>).
  N.I.N.A., KStars/Ekos, CCDciel, Sequence Generator Pro, Voyager, APT
  and SharpCap all drive PHD2 through it. PHD2 documents 48 methods;
  N.I.N.A.'s PHD2 client can send 21 of them and needs ten to start,
  dither and stop guiding. Celestron's CPWI serves a "PHD2 Server" so
  that such clients can use its own guider.
- **The guide log format**
  (<https://github.com/OpenPHDGuiding/phd2/wiki/PHD2GuideLog>), read by
  PHD2 Log Viewer (<https://github.com/agalasso/phdlogview>) and other
  tools. CCDciel's internal guider writes its log "in "PHD2" format".

A guider that wants to be useful outside rusty-photon speaks one or
both of these. Neither needs anything from ASCOM.

## Options

### 1. The engine

| | What it is | What it removes from the cost table | What remains or is added |
|---|---|---|---|
| **E0** | Stable PHD2, as today, plus the Windows session keeper | Nothing | Everything, and the keeper still to build |
| **E1** | PHD2's development build, using the rusty-photon drivers through Alpaca | The facade and the device-list exception, if PHD2 takes its camera from the vendor driver | The desktop session, the GUI profile, the PHD2 client and its mock, the rotator bridge (its Alpaca client covers a camera and a mount). A development build on the rigs. Whether its Alpaca client speaks HTTPS and Basic Auth is not verified |
| **E2** | A headless PHD2: the fork above, or one of our own | The desktop session, once it exists | The fork is still the windowed binary and targets arm64 Debian only. Our own would mean owning 107,750 lines of C++ whose logic is interleaved with wxWidgets (PHD2's hysteresis algorithm is 373 lines around a five-line update), with an upstream that takes no outside pull requests |
| **E3** | INDIGO's guider agent | The desktop session: INDIGO is a headless server with a permissive licence. It has multi-star selection, full-frame correlation, five RA controllers | A second server, a second protocol and a wrapper like `phd2-guider`. To use the rusty-photon drivers it needs `indigo_system_alpaca`, which its README calls "Under development", tested "with simulators only", without HTTPS or authentication, and which disconnects an Alpaca device "for all other clients" when its INDIGO device disconnects. INDIGO on Windows is a public beta since July 2026 |
| **E4** | A native Rust guider | Every row except the intrinsic ones (§ What goes away) | The guider itself: the code is ours, and so is every failure |

Three more were considered and set aside. **Guiding hardware** with its
own computer (Lacerta MGEN-3, ZWO ASIAIR, Celestron's StarSense
Autoguider) is closed and not controllable from `rp`. **An encoder
mount** needs no guider and is a different rig. **Guiding on the
science frames** (correcting once per main exposure from the main
camera's own images) removes the guide camera altogether and is how
survey telescopes use the Donuts method, but it cannot correct anything
faster than one exposure, which rules it out on a mount whose periodic
error moves the star within a sub.

**Recommendation: E4.** E3 is the serious alternative: it is real,
headless and maintained. It loses on integration, not on guiding: every
route from INDIGO to our devices either bypasses the drivers this
project exists to provide or goes through a bridge its authors call
unfinished. E1 is cheap to try and worth trying on pier1 for what it
teaches about guiding the GTi through its Alpaca driver, but it leaves
the desktop session in place.

### 2. Where a native guider lives

The core is a library crate in every option below: frames and
timestamps in, pulse requests and statistics out, no I/O, time passed
in. The question is what hosts it.

| | Host | Frames and pulses travel | For | Against |
|---|---|---|---|---|
| **L1** | A standalone service | Directly: the service is an Alpaca client of the camera and the mount | Guiding survives an `rp` restart. The loop shares no process with `rp`'s other work. It replaces `phd2-guider` at the seam `rp` already has. Usable without `rp` | A second Alpaca client of two devices. One more service to package and supervise |
| **L2** | `rp` itself, as built-in code | Through `rp`'s existing device connections | No new service or client. `rp` already holds both connections and the image kernels | An `rp` restart, crash or config apply stops guiding mid-exposure. `rp`'s capture path is shaped for science frames (a FITS file and a sidecar, synced to disk, per frame; no subframe). The guider cannot be used without `rp` |
| **L3** | A tool provider or orchestrator | Through `rp`'s MCP tools | Uses `rp`'s leases, events and safety as they are. [`focus-model`](../services/focus-model.md) and `calibrator-flats`, tool providers, and [`polar-align`](../services/polar-align.md), an orchestrator, are the precedents for a first-party loop driven through `rp`'s tools | Needs new `rp` tools for a guide pulse and for a capture that writes no file. Every frame and every pulse passes through `rp`, so the loop stops with `rp`, like L2 |
| **L4** | A camera driver | In-process from the SDK | The shortest frame path | Ties guiding to one vendor, makes a driver a client of the mount, and returns to PHD2's ownership problem. Rejected |

How the options sit against the written rules:

- **`rp`'s sorting rule**
  ([rp.md § Component Categories](../services/rp.md#component-categories))
  puts algorithms that are "pure Rust math" inside `rp` and makes a
  component an rp-managed service "only when they must wrap an external
  program … that has its own crash and restart behavior". Read
  literally that is L2. The rule's examples (auto-focus, centering) are
  tool calls that start, run and return. A guider is a control loop
  that runs for hours with its own device traffic, and the documents do
  not say where such a thing belongs. The rule's own stated boundary,
  "process supervision and lifecycle role, not authorship", points at
  L1: the guider's lifecycle is not `rp`'s.
- **`rp`'s tenet 4**, "Remote interfaces only", holds under L1 and
  improves: the guider speaks Alpaca to devices and HTTP to `rp`, and
  "the one exception to the Alpaca-only rule" goes away.
- **The mount motion gate**
  ([rp.md § Mount Motion Gate](../services/rp.md#mount-motion-gate))
  exempts guide pulses because "PHD2 corrects the mount directly". L1
  keeps that model exactly. L2 and L3 would route pulses through `rp`
  and need the gate's rules rewritten.
- **`rp` is the only client that commands devices today.** Under L1 the
  guider commands the guide camera (exclusively while it loops) and
  sends the mount pulses and nothing else. Who has the guide camera
  needs a rule of its own, and today's two leases do not give one:
  `rp`'s train lease lasts ten minutes at most and binds only `rp`'s
  own callers, and the facade's sweep lease holds PHD2 off for one
  focus sweep. Alpaca has no session that tells two clients apart.
  `rp` keeps its own captures through the guide camera and the
  guider's runs apart, with a guard in its memory
  ([rp.md § The guide camera guard](../services/rp.md#the-guide-camera-guard),
  decision 14).

A variant of L1 sends frames directly and pulses through `rp`, so that
`rp` stays the mount's only commander. It puts `rp` on the path of
every correction and stops guiding when `rp` restarts, for no gain the
pre-pulse checks in § Recommended shape do not already give.

**Recommendation: L1**, recorded as an ADR that extends the sorting
rule to first-party long-running control loops. Because the core is a
library, the placement can be revisited without rewriting the guider.

**Decided 2026-11-12: L1.** The owner weighed L3 first and settled on
the standalone service.
[ADR-024](../decisions/024-native-guider-as-a-standalone-alpaca-client-service.md)
records the decision and extends the sorting rule.

### 3. What it presents to the ecosystem

These are not exclusive.

| Surface | Who uses it | Cost | Recommendation |
|---|---|---|---|
| The HTTP contract in `crates/rp-guider` | `rp`, the safety enforcer, sentinel's health probe | Exists. Needs the PHD2 names taken out and a few members added (§ Recommended shape) | **Required** |
| A setup page ([ADR-023](../decisions/023-services-configure-themselves-on-their-own-setup-pages.md)) | The operator | The house pattern | **Required**; it replaces PHD2's wizard |
| PHD2-format guide log | PHD2 Log Viewer and every other log tool; a person diagnosing a night | PHD2's writer is about 700 lines | **From the first version** |
| A live view for a person: star profile, error graph | The operator, through `ui-htmx` | A page reading the metrics endpoint | After the first sky run; it replaces PHD2's window |
| PHD2-compatible event server on TCP 4400 | N.I.N.A., Ekos, Sequence Generator Pro, Voyager and the rest, with no change on their side | About ten methods for N.I.N.A.; PHD2's server is about 3,000 lines for 54 | **Later, optional.** It is what makes the guider useful to someone who does not run `rp`; CPWI is the precedent |
| An MCP tool provider | LLM and UI clients | `rp` already proxies the guider tools | Not needed |
| A N.I.N.A. plugin | N.I.N.A. users | C#, another repository; <https://github.com/aduffeck/pins-guider> does this for a PHD2 port | Not ours to build; the event server covers N.I.N.A. |
| An Alpaca device of an invented type, or a stand-in device carrying `Action`s | Nobody: no client would discover or drive it | — | **Rejected** |

### 4. Measurement

| | Method | Evidence | Note |
|---|---|---|---|
| **M1** | One star, centroid in a search region | The classic. PHD2's defaults: 15 px search region, SNR floor 3, 50 % star-mass change counts as lost | Fails with the star |
| **M2** | Several stars, combined | No controlled measurement of the gain exists; the best figure is a forum test of "about 20%" (<https://groups.google.com/g/open-phd-guiding/c/6JrKbgzmNXg>). Every current guider has it, and they disagree on the combining rule: PHD2 an SNR-weighted mean of up to 9 stars; INDIGO a mean after 1.5σ rejection; CCDciel a plain mean after rejecting outliers around the median, with a source comment that a weighted mean "gives poorer result"; Ekos a median with star-pattern matching and a virtual guide star when the real one drops out | Averages seeing across the field; survives the loss of any one star |
| **M3** | Full-frame correlation (Donuts) | McCormac et al. 2013 (<https://arxiv.org/abs/1304.2405>): residual below 0.2 px from under-sampled to badly defocused stars, 1–15 ms per frame, on-sky 0.04 px RMS on NGTS. INDIGO and lin_guider implement it | Needs no star selection; works defocused and in crowded fields. Needs only an FFT crate (`rustfft`, MIT OR Apache-2.0) |
| **M4** | A plate solution per frame | Professional telescopes only (SDSS-V); no amateur guider closes its loop on one | A solve gives the camera's angle, scale and parity, not the mount's response. Useful for calibration (§ 5), not for the loop |

The workspace already has the single-frame kernels, which `rp` uses:
`detect_stars`, `fit_2d_gaussian`, `measure_listed`,
`estimate_background` and the HFR and SNR measures, generic over the
pixel type, in `crates/rp-image-analysis`. `crates/rp-guiding` measures
the frame-to-frame shift of several stars with them. What does not
exist anywhere in the workspace: dark subtraction or a persistent defect
map. Detection copies
the frame to `f64` and filters it, so the per-frame path should detect
once at star selection and afterwards measure only at the listed
positions. What the kernels cost on guide-sized frames on the Pi 5 is
in § Staging, under "What G1 found".

pier1's guide camera is a colour sensor with 1.45 µm pixels. Summing
each 2 × 2 Bayer cell gives a luminance frame and a sensible sampling;
the kernels handle mono frames only today.

**Recommendation: M2 from the start**, written for N stars with one
star as the degenerate case, a median or rejected mean as the combining
rule, and the choice between rules made on recorded frames. **M3 as the
second mode** once the first works: it is cheap and covers the cases
star selection handles worst. To get there the kernels move from the
`rp` service crate into a library crate both services use.

### 5. Calibration

Calibration is where a guider fails in the field, and it spends dark
time.

| | Method | Dark time | Risk |
|---|---|---|---|
| **C1** | Step calibration: pulse west until the star has moved far enough, come back, clear Dec backlash, step north, come back. PHD2 aims for 12 steps over 25 px; INDIGO sizes its steps automatically and allows 20 | Minutes | Proven. Sensitive to backlash, to a wrong guide-speed assumption and to calibrating near the pole |
| **C2** | Predicted, then verified: the angle and parity from a plate solution of one guide frame, the rates from the mount's guide rates, Dec and the pixel scale; then one pulse on each axis to confirm sign and rate | Seconds | The prediction is only as good as the reported guide rate. A solve cannot see backlash |
| **C3** | Reuse: keep one calibration per train and transform it by Dec, pier side and rotator angle. CCDciel: "the calibration does not age"; INDIGO needs no recalibration after a GOTO or a flip | None | Stale after any change to the train the guider is not told about |
| **C4** | Refinement from guiding residuals | None | A research item |

A native guider has an advantage over every guider that owns its own
drivers: through Alpaca it knows the guide rates, the declination, the
pier side and the rotator angle, and through
[`plate-solver`](../services/plate-solver.md) it can get the camera
angle and parity. PHD2's sanity checks become stronger with that
knowledge: a measured rate can be compared with the expected one, not
only with the other axis. Its published thresholds are the starting
values: an alert when the axes are more than 12.5° from orthogonal or
the ratio of the two rates is more than 0.20 from the cos(Dec) it
expects, and calibration within about 20° of the celestial equator.

Calibration moves the mount. Under tenet 3 it runs only on an explicit
request, never because the service started, a device reconnected or a
config was applied.

**Recommendation: C1 first**, with every Alpaca-known quantity used as
a cross-check, and **C3 with it** so a rig calibrates rarely. **C2
next**, measured against C1 on the same rig: it is the option that buys
night time (tenet 1).

### 6. Control

What four guiders ship as defaults:

| | RA | Dec | Dead band | Pulse cap | Exposure |
|---|---|---|---|---|---|
| PHD2 | Hysteresis: 0.1 of the last move blended in, aggression 0.7 | Resist Switch: aggression 1.0, reverses only on sustained error | `max(0.1515 + 0.1548 / scale, 0.15)` px | 2,500 ms | 1–3 s to start; 2–4 s to smooth seeing |
| INDIGO | PI: P 80 %, the I term inactive at the default stack size | The same, with direction modes | 0 px; 10 ms minimum pulse | 1,000 ms | 1 s |
| Ekos | Proportional 0.75, integral 0 | The same | 0.2″ minimum pulse | 25″ | — |
| CCDciel | Gain with hysteresis, "typically" 30 % | Gain with hysteresis 70 % and a reversal filter | 40 ms minimum pulse | 2,500 ms | — |

They agree: a proportional response below one, a dead band, a cap, no
integral term by default, and reluctance to reverse Dec. The PHD2
developers say the same in prose. Their best-practices slides
(<https://openphdguiding.org/PHD2_BestPractices_2019-12.pdf>): "You
can't correct for seeing" and "under-correction is better than
over-correction, especially for Dec". Their log-analysis tutorial
(<https://openphdguiding.org/Analyzing_PHD2_Guide_Logs.pdf>):
"elaborate guiding algorithms or process control models have trouble
producing any significant improvements over simpler algorithms". Each of these controllers is a
few lines of arithmetic. No generic PID crate helps: the guiding parts
are the outlier gate, the dead band, the cap and the unit conversion.

Predictive periodic-error correction has thin evidence: a 22.64 % RMS
reduction on an indoor test rig in the paper behind PHD2's
implementation (<https://doi.org/10.1109/TCST.2015.2420629>), a
simulation figure from INDIGO, and no quantified on-sky gain found.

What the rusty-photon drivers add and a generic guider cannot know: the
GTi implements a pulse as rate-shifted tracking; one RA step is about
0.357″, or 47 ms of pulse at half sidereal rate; and a west pulse
shorter than 71 ms delivers only edge steps
([star-adventurer-gti.md § PulseGuide lifecycle](../services/star-adventurer-gti.md#pulseguide-lifecycle)).
A dead band below the mount's own granularity commands noise. How the
guider learns a driver's granularity (config, or a driver `Action`) is
a design question. Dec backlash on the GTi is unmeasured.

**Recommendation:** hysteresis on RA and resist-switch on Dec with
PHD2's published defaults, a dead band no smaller than the mount's pulse
granularity, a pulse cap, one-direction Dec as a mode, and a backlash
measurement at calibration. Predictive PEC only after a logged night
shows a periodic residual worth removing.

### 7. The frame path and the guide camera

A guide frame through Alpaca is one `StartExposure`, a poll of
`ImageReady` and one image download. What is known about its cost:

- With PHD2's simulator on pier1, exposures of 1, 10 and 100 ms through
  the facade complete 87, 84 and 172 ms after the request. For the
  QHY5III715C at full frame,
  the client has the frame 0.8 s after the exposure ends (PHD2 0.5 s,
  facade 0.1 s, download 0.2 s).
- Single-shot conformance runs show a 2 s exposure reaching `ImageReady`
  after 2.5–4.5 s, on differing hosts and build profiles. Both validated
  QHY bodies take about 4 s, and no document explains the extra 2 s.
- **No sustained-cadence measurement exists** for any camera service:
  no frames per second, no exposure-end-to-client latency.

A correction is as old as the exposure plus that latency, so a
half-second of overhead on a 2 s exposure is tolerable and two seconds
is not.

| | Path | Note |
|---|---|---|
| **F1** | Standard one-shot exposures; find and remove the overhead in the drivers | A 2 s unexplained delay is a driver defect whatever the guider is |
| **F2** | A non-standard streaming endpoint on the camera services | Only if F1 fails. Leaves the standard, and other Alpaca cameras would not have it |
| **F3** | The guider links the camera SDK | Rejected: it is L4 again |

Multi-star guiding wants the whole field and Alpaca gives one region
per exposure, so the frame is either the full sensor, binned (a
QHY5III715C frame is 17 MB at 16 bit unbinned, 4 MB binned 2 × 2), or
the bounding box of the chosen stars.

A camera has one exposure in flight, one settings cache and one image
slot for all its clients. The guider therefore has the guide camera to
itself while it loops, writes region, binning and gain itself when it
starts to expose instead of assuming them, and gives the camera up when
stopped. Who has the camera was first designed as a camera lease the
guider serves and `rp` takes (ADR-024, decision 6), since `rp`'s train
lease cannot do it alone: it binds only `rp`'s own callers, for ten
minutes at most. On 2026-11-15 the owner settled it the other way
(decision 14,
[ADR-026](../decisions/026-rp-keeps-its-guide-camera-captures-and-the-guider-apart.md)):
the guider serves no camera lease, and `rp` keeps its captures through
the guide camera and the guider's runs apart on its own. Its captures
through the camera and a guiding train's lease go ahead only with the
guider read `idle`, and they and a start of guiding refuse each other
at once
([rp.md § The guide camera guard](../services/rp.md#the-guide-camera-guard)).
What that binds is again only `rp`'s own calls, and the owner accepted
it: a client of the guider's API other than `rp` can still start the
guider while `rp` exposes.

Neither rig's guide camera has a validation record under `qhy-camera`.
For a mount without usable `PulseGuide`, `zwo-camera` has an
asynchronous ST4 `PulseGuide`
([zwo-camera.md § Pulse guiding](../services/zwo-camera.md#pulse-guiding));
`qhy-camera` has none.

**Recommendation: F1**, with the cadence measured before anything else
is built (§ Staging, G1).

### 8. Guide-camera focus

With a native guider the guide camera is an ordinary Alpaca camera
served by its vendor driver, and the guiding train is an ordinary
train:

- **Before guiding**, focus-model's `focus_train` sweeps it through
  `rp`'s ordinary capture path, with gain and binning available. The
  facade, its capture slot and its sweep lease are not needed.
- **While guiding**, the guider measures the half-flux diameter of its
  guide stars on every frame with the same kernels `rp` uses, and
  reports it in the metrics it already serves. `rp`'s
  [Guide Focus Watch](../services/rp.md#guide-focus-watch) and the
  guide-train `auto_focus` read it as they read PHD2's today.
- The guider can also report HFD while it loops without guiding, which
  PHD2 cannot. Whether the second mechanism is then still needed is a
  design question for the guiding train.

[`pa-scops-oag`](../services/pa-scops-oag.md), the driver for the
off-axis guider's focuser, is unaffected. What stays hard is physical:
few stars in an off-axis field, and the guide focus moving with the
main focuser.

### 9. Testing and validation

PHD2 cannot run in CI, so today's tests run against a mock of it, and
nothing in CI closes a guide loop. A native guider can be tested at
every level:

1. **The core, as pure functions.** Unit and property tests on
   centroiding, combining, calibration arithmetic and each controller.
2. **A closed-loop simulator.** A mount model (polar-alignment drift,
   periodic error, Dec backlash, a guide rate that differs from the
   reported one, pulse granularity) and a star-field renderer at
   sub-pixel resolution with seeing, run in simulated time so a night
   takes seconds. It answers "does it converge, does it oscillate, does
   it recover" for thousands of mount and sky conditions. The
   `guide-simulator` crate holds both
   ([its design](../crates/guide-simulator.md)). PHD2's own simulator
   physics is about 700 lines.
3. **Replay of recorded frames.** Guide-camera frames recorded on a rig
   through `qhy-camera`, the mount tracking and nothing guiding, are
   fixtures: the native measurement runs over real stars, real noise
   and the sensor's own defects. Nothing gives the true offset of such
   a frame, so the replay shows that the measurement holds its stars
   and that the stars agree with one another, and not that an offset is
   right. Frames cropped from rig2's hour of 2026-11-22 are checked in
   with `guide-replay` and replayed in CI.
4. **The service, under BDD**, against
   [`guide-simulator-alpaca`](../crates/guide-simulator-alpaca.md)'s
   Alpaca Camera and Telescope, which serve the simulator on the wall
   clock.
5. **On sky**, pier1 first because the mount driver is ours, then rig2.

The acceptance is the owner's, of 2026-11-14: "We really only need
guiding that is good enough to get us good images." No stage measures
the guider against PHD2, on the same night or from its logs. The
acceptance list:

- no run-away under any simulator condition;
- recovery from a lost star and from a passing cloud;
- tight stars in an evaluation session. The main camera takes guided
  subs at the rig's usual exposure and, in the same hour, exposures of
  the same field short enough that the mount's error does not widen
  their stars. `rp`'s
  [`measure_stars`](../services/rp.md#measure_stars-contract) measures
  both on the same stars, and the median HFR of the subs is within
  15 % of that of the short exposures;
- a full night guided unattended, run as any imaging night and taking
  no frames to compare with. The median eccentricity of the stars in
  its subs, by `measure_stars`, is at most the rig's own limit,
  `target_store.default_grading.max_eccentricity`
  ([rp.md § Progress derivation](../services/rp.md#progress-derivation)),
  or 0.6 where the rig sets none. A target's own grading does not
  change the limit.

The short exposures are for evaluating the algorithm and not for
operation, in the owner's words of 2026-11-14: "For operations we will
rely on HFR from the actual exposures we are taking all the time. For
evaluating the algorithm, we can use short exposures - see tenet 1".
Outside an evaluation session a rig takes no frames to compare with
(tenet 1), and how well it guides shows in the HFR of the subs
themselves. Where a rig grades its frames, the `grading` section of
each frame's sidecar carries that HFR.

The 0.6 and the 15 % are starting values of this plan.

### 10. Port or clean implementation

| | Approach | Obligation | Trade |
|---|---|---|---|
| **P1** | A clean implementation from published descriptions: PHD2's manual and best-practice papers, the Donuts and predictive-PEC papers, observed behaviour | None | Re-learns the edge cases on sky. Free to be shaped for testing |
| **P2** | Port selected PHD2 routines | PHD2 is BSD-3-Clause with per-file copyright holders: each ported file keeps its notice in source and in binary documentation, and the names are not used for endorsement | Carries the field knowledge, and with it a structure built around a GUI. <https://github.com/aduffeck/pins-guider> describes itself as "a faithful port of PHD2" |
| **P3** | Port INDIGO's agent | A permissive two-clause licence; one source header says "Code is generated by AI" for its Alpaca part | Smaller and headless by design; less field history than PHD2 |

Ekos (GPL-2.0-or-later) and CCDciel (GPL-3.0) can inform by their
documentation only. The workspace is `MIT OR Apache-2.0`, has no
licence tooling, and states "behavioural reference only, no code is
copied" per service, not as a general policy.

**Recommendation: P1 for the structure**, with the published numbers of
PHD2 and INDIGO as starting values (the tables above), and a decision
per routine where behaviour is subtle enough that a port with
attribution is the safer way to get it right. Whether a description of
an algorithm is free of licence obligation is a legal judgement for the
owner.

## Recommended shape

[`guider.md`](../services/guider.md) specifies this shape in full, and
where this section and the design differ, the design holds.

```
                         rp (11115)
      start · stop · dither · stats │ HTTP, the rp-guider contract
                                    ▼
   ┌───────────────────────────────────────────────────┐
   │ guider service (11129)                            │
   │   HTTP API · setup page · guide log · metrics     │
   │   ┌───────────────────────────────────────────┐   │
   │   │ guiding core (library, no I/O)            │   │
   │   │ select · measure · calibrate · control    │   │
   │   └───────────────────────────────────────────┘   │
   └────────┬───────────────────────────────┬──────────┘
     Alpaca Camera                    Alpaca Telescope
     StartExposure · ImageArray       PulseGuide · IsPulseGuiding
            ▼                               ▼
     qhy-camera, zwo-camera, …        star-adventurer-gti, any Alpaca mount
```

**Pieces.** The names are the ones ADR-024 and the design use.

| Piece | Kind | Holds |
|---|---|---|
| `rp-guiding` | Library crate | The core: star set, measurement and combining, calibration, controllers, dither and settle, the state machine. No I/O, no runtime, time passed in |
| `rp-image-analysis` | Library crate | The kernels `rp` uses, plus dark and defect handling when those are built |
| `rp-guider` | Existing contract crate | The HTTP contract and its client, as the design specifies the contract |
| `guider` | Service, port 11129 | The Alpaca clients, the loop, the HTTP API, the setup page, the guide log |
| `guide-simulator` | Test-infrastructure crate | The mount model and the star renderer |
| `guide-simulator-alpaca` | Test-infrastructure crate | The simulator behind an Alpaca Camera and an Alpaca Telescope |

**Behaviour the tenets fix in advance.**

- **Idle after every start.** The service never calibrates, loops or
  guides because it started, restarted, reconnected to a device or had
  its config applied (tenet 3). Reacquiring a lost star inside a
  guiding run the operator started is part of that run; resuming a run
  after the service's own restart is not.
- **Checks before every pulse.** The guider reads `Tracking`, `Slewing`
  and `AtPark` and sends nothing unless the mount is tracking, still and
  unparked. The GTi driver accepts a pulse with tracking off, so the
  check cannot be left to the driver. A failed check stops guiding and
  reports; it does not wait and resume.
- **Bounded authority.** Every pulse is capped, no pulse is sent while
  the star is lost, and corrections that accumulate in one direction
  without the star following stop the run.
- **Stop always works.** Stop and dither-abort are accepted in every
  state and do not wait for a frame.
- **It never changes `Connected`.** `rp` owns device connections; the
  guider requires its devices connected and reports otherwise.
- **Config is validated at load** (tenet 2), and the setup page is how
  it is changed.
- **It keeps guiding if `rp` goes away**, as PHD2 does today, and the
  pre-pulse checks stop it when someone else parks or slews the mount.

**Changes to the contract.** The contract in `crates/rp-guider` becomes
the one [guider.md § HTTP API](../services/guider.md#http-api)
specifies. Against the contract `phd2-guider` serves
([phd2-guider.md § HTTP API](https://github.com/rusty-photon/rusty-photon/blob/865fe440e74ebc305269774bac4d9b9ac9da44f1/docs/services/phd2-guider.md#http-api)):

| `phd2-guider`'s contract | The design's contract |
|---|---|
| `GET /api/v1/equipment`, PHD2's equipment list passed through | `GET /api/v1/devices`: what the guider knows of its camera and its mount. Its `rotator` is always `null` |
| `app_state`, PHD2's state name, in the stats; one acknowledgement word per endpoint (`stopped`, `paused`, `resumed`, `cleared`, `selected`) | `state`, one of the guider's six states, in the stats and in every answer to a mutating request |
| `POST /api/v1/phd2/restart`; the codes `phd2_unreachable`, `restart_failed`, `restart_unsupported`, `guiding_active`, `camera_busy`, `camera_connected` | Gone. The guider wraps no program, and it is restarted as any service is |
| `calibration/clear` takes `which` and is accepted while guiding | It empties the whole store and is refused in a run state |
| Pixels only | Arcseconds beside pixels in every result, since the pixel scale is known from config. A request takes either |
| A calibration only as part of `guiding/start` | `POST /api/v1/calibrate` and `GET /api/v1/calibration` as well |
| No dither abort | `POST /api/v1/dither/abort`, and the code `dither_aborted` |
| The facade's sweep lease: ASCOM `Action`s on a second port | Nothing on the guider's API: `rp` keeps its captures through the guide camera and the guider's runs apart on its own (decision 14) |
| — | `POST /api/v1/loop`, `GET /api/v1/frame/latest`, `last_stop` in the stats, the code `stopped` for a request a stop cut short |

**Changes to `rp`.** `rp` speaks one guider contract, and
`GuidingConfig` gains no backend selector (decision 7). The guiding
train's camera becomes an ordinary roster camera, on which `rp` exposes
only with the guider read `idle`, under a guard of its own that a
start of guiding cannot take while a capture or a guiding train's lease
holds it (decision 14). A guiding train's train lease takes that guard
at the second step of its acquire. The
motion gate's text about pulses names the guider, not PHD2.
`restart_guider` goes. The rotate-while-guiding ladder
([rp.md § Rotator Tool Details](../services/rp.md#rotator-tool-details))
loses its rotator branch, and its calibration step changes: the guider
refuses `calibration/clear` in a run state, so a rotation past the
threshold stops guiding, clears the calibration and starts guiding
again, which calibrates. A rotation of the guiding train while guiding
is not active has to clear the calibration as well: the guider keeps
its record from run to run and cannot see the field turn. The guider's
devices and `rp`'s guiding train are configured apart, and a guider on
a camera that is not the train's collides with `rp`'s captures through
that camera, so `rp` has to tell that both name the same camera and
mount before it starts a run: the guider reports the
`UniqueID` of each of its devices, and `rp` compares it with its own
([rp.md § Guider Service](../services/rp.md#guider-service)). The
guider waits in `star_lost` for as long as the run lasts and never
selects stars again by itself, and it pushes no event when it stops a
run. So `rp` watches the run and reports a loss that lasts and a run
that ended without `rp` asking
([rp.md § Guide Run Watch](../services/rp.md#guide-run-watch)), and
the night's document asks for a new selection after the first and
acquires the target again after the second
([session-runner.md](../services/session-runner.md#deep_skyjson-the-night-cycle-document)).
`rp.md` holds the design of these changes, and stage G6 makes them
(§ Staging).

**Joining the workspace.** A service that is not an Alpaca device joins
sentinel's `NON_ALPACA_SERVICES` list and about ten other hand-kept
lists (the doctor catalog, the packaging port table, the Bazel service
lists, the package directory, the units, the Windows installer
fragment).

## What goes away, what stays, what is lost

**Goes away, when `rp` moves to the new contract (G6):** PHD2 and its
build from source; its desktop session on every platform, including the
Windows keeper, which is not built; the profile wizard; the PHD2
client, the facade, the mock and their tests (about 5,600 production
lines by whole file, 46 facade scenarios); the device-list exception
for the guide camera; the protocol exception in `rp`'s design; the
rotator bridge backlog item; the wedge-driven restart route, which
becomes an ordinary service restart.

**Stays:** `rp`'s guider tools and a guider contract in
`crates/rp-guider`; RMS statistics; the Guide Focus Watch; the train
lease; `pa-scops-oag`; everything that is hard about guiding itself.

**Lost until rebuilt:** PHD2's field maturity; the Guiding Assistant;
predictive PEC; PHD2's window as a debugging tool; help from the PHD2
community, which supports PHD2 and not logs from another guider.

**Carried for the transition:** nothing in `rp`. `rp` speaks one
contract at a time, with no backend selector and no second client
(decision 7). The cost falls on the rigs instead: from G6 on, a rig
that is not yet brought up on the native guider has no guider under a
current build (§ Decisions for the owner, 11). G6 runs first, so that
is pier1 until G5 and rig2 until G7.

## Staging

G0 and G6 are merged: G0's record is § Where the rigs stand, and what
it changes for the later stages follows the table; G6 took PHD2's
pieces out of the tree and moved `rp` to the native contract. G1 is
merged: both rigs were measured on 2026-11-17, and what G1 found
follows what G0 changes. G2 is merged: the guider's core guides
`guide-simulator`'s mount in closed loop in CI. G3 is merged: the
measurement held its guide stars through an hour recorded on rig2
(decision 15), within the replay's tolerance. G4 is merged: the
`guider` service is built and packaged, and `rp`'s guiding runs against
it and the simulator's devices in CI. G5 is in progress. G7a is in
progress: rig2's mount answers over Alpaca through ASCOM Remote Server
in the owner's desktop session, and `rp` there binds it (decision 13).
G7 has not started. No stage measures the guider
against PHD2, and no new work is started for PHD2 (decisions 6 and 12).

The stages run in the order of the table. G6 comes right after G0, as
the owner settled on 2026-11-14 (decision 11): PHD2's pieces leave the
tree, and `rp` speaks the native contract, before the guider is built.
G7a is the stage decision 13 adds. It needs nothing of G5, and runs
beside it.

| Gate | Work | Passes when | Status |
|---|---|---|---|
| **G0** | Record how each rig guides today, while PHD2 is still installed: its profile, how it reaches the mount, guide logs from each rig. The logs become the fixtures the guide-log writer is tested against | The "not recorded" items in § Where the rigs stand are closed: how PHD2 drives each mount, which the section records for both rigs | Merged 2026-11-14: [stage G0](https://github.com/rusty-photon/rusty-photon/pull/1780) |
| **G6** | PHD2's pieces leave the tree, and `rp` speaks the native contract. `phd2-guider`, its facade, its mock, its packaging, `restart_guider` and the old contract types go. The types and the client of the contract [`guider.md`](../services/guider.md) designs take their place in `crates/rp-guider`, and `rp`'s guiding tools move to them, tested against a test double of that contract. No compatibility is kept with the old contract or its configuration. Four pull requests, in this order. **The design:** `rp.md`, `focus-model.md`, `session-runner.md`, `sentinel.md`, `guider.md` and [`daily-power-cycle.md`](daily-power-cycle.md) say what `rp` and its plugins do against the native guider, and mark it designed and not yet built where the code still speaks PHD2's contract. In `rp.md` that is § Guider Service with the check that the guider's devices are `rp`'s and the watch over a run, the train lease's camera-lease step, the capture through the guide camera, the motion gate's text about pulses, the rotate-while-guiding ladder and the end of `restart_guider`, among others; in `focus-model.md`, the training runs on a guiding train; in `session-runner.md`, the night document's two rules for a lost star and for a run the guider stopped; in `sentinel.md`, the units the `phd2-guider` package ships for PHD2 and its display; in `guider.md`, the `UniqueID` the guider reports for each device; in `daily-power-cycle.md`, decision 10 and PHD2's restart in stages P2 and P4. **The BDD scenarios** of that design, against the double. **The code:** the contract's types and client in `crates/rp-guider`, the double in `bdd-infra`, and what the design changes in `rp`, in `focus-model`, in the night document and in the cards `ui-htmx`'s stream draws for `guide_stopped` and `guide_star_lost`. **The removal:** `services/phd2-guider` with its packaging and its entries in CI, in doctor's catalog and in sentinel, and with it `phd2-guider.md` and what the other living documents say of the removed pieces, the packaging guides, `doctor.md`, `README.md` and `workspace.md` among them | `rp`'s BDD green against the double, and nothing of PHD2 left in the tree to build or to package | Merged 2026-11-17. The design merged 2026-11-14: [stage G6, the design](https://github.com/rusty-photon/rusty-photon/pull/1785). The BDD scenarios merged 2026-11-15: [stage G6, the scenarios](https://github.com/rusty-photon/rusty-photon/pull/1789). The code merged 2026-11-16: [stage G6, the code](https://github.com/rusty-photon/rusty-photon/pull/1795). The removal merged 2026-11-17: [stage G6, the removal](https://github.com/rusty-photon/rusty-photon/pull/1807) |
| **G1** | Cadence spike, no guider: loop 2 s exposures through `qhy-camera` on each guide camera, full frame binned and subframe; time short pulses through the GTi driver; time the kernels on the Pi 5. Both guide cameras get a validation record under `qhy-camera` | Exposure end to pixels in the client well under the exposure length, sustained for an hour. If not, the driver overhead is fixed first | Merged 2026-11-18: [stage G1](https://github.com/rusty-photon/rusty-photon/pull/1821). Both guide cameras pass, measured on 2026-11-17 (below, "What G1 found") |
| **G2** | The kernels move to `rp-image-analysis`. `rp-guiding` and `guide-simulator`: multi-star measurement, step calibration, the two controllers, dither and settle. Five pull requests, in this order. **The kernels:** `crates/rp-image-analysis` holds the image kernels, and `rp` uses them from there. **The simulator:** `guide-simulator`'s mount model and star renderer, stepped in simulated time: drift, periodic error, backlash, short pulses delivered as coarsely as G1 measured on the GTi, seeing and clouds. The Alpaca devices in front of them wait for G4, the first stage that talks to them. **The measurement:** `rp-guiding`'s star selection, several stars measured each frame and combined, the flux test and the star-lost frame, with the frame-wide σ made cheap. **The calibration:** the legs, the step, the cross-checks, the Dec rules, and when a stored record is used. **The control:** the two controllers, the minimum move, the pulse floor and cap, the runaway stop, dither and settle, and the closed-loop runs against the simulator that the stage passes on | The loop converges and recovers across the simulator's mount and sky conditions in CI | Merged 2026-11-22. The kernels merged 2026-11-18: [stage G2, the kernels](https://github.com/rusty-photon/rusty-photon/pull/1824). The simulator merged 2026-11-19: [stage G2, the simulator](https://github.com/rusty-photon/rusty-photon/pull/1835). The measurement merged 2026-11-19: [stage G2, the measurement](https://github.com/rusty-photon/rusty-photon/pull/1840). The calibration merged 2026-11-21: [stage G2, the calibration](https://github.com/rusty-photon/rusty-photon/pull/1855). The control merged 2026-11-22: [stage G2, the control](https://github.com/rusty-photon/rusty-photon/pull/1856). The closed-loop runs pass in CI: drift and periodic error, the GTi's coarse pulses and backlash, a cloud, a gust under a fast drift and a dither's settle, and the guider pulses no more at a mount that stops following the pulses or reverses them |
| **G3** | Replay: record guide-camera frames on sky on rig2 through `qhy-camera`, the mount tracking and nothing guiding, and run the measurement over them. The owner points rig2's mount and starts it tracking, since nothing of the workspace reaches it before G7a (decision 15). Two pull requests, in this order. **The replay:** [`guide-replay`](../crates/guide-replay.md), the recorder `record.ps1` beside it, and the tolerance, stated before the recording. **The recording:** an hour of the QHY5III678M binned 2 × 2, its validation record with the replay's report, and frames cropped from it that a test replays in CI | The measurement holds its guide stars through the recording, and the stars' offsets agree with one another frame by frame within [`guide-replay`'s tolerance](../crates/guide-replay.md#the-report): at least four guide stars, no frame lost while its stars are there, each star's scatter at most 0.5 px of the binned frame on each axis, and at most one deviation in 1,000 at 2 px or more from its star's mean | Merged 2026-11-24. The replay merged 2026-11-22: [stage G3, the replay](https://github.com/rusty-photon/rusty-photon/pull/1865), with the measurement's position rule: [a star where the others place it](https://github.com/rusty-photon/rusty-photon/pull/1867). The recording merged 2026-11-24: [stage G3, the recording](https://github.com/rusty-photon/rusty-photon/pull/1878). The hour on rig2 of 2026-11-22 is within the tolerance: 8 guide stars usable in all 1,441 frames, each scattering 0.11 px at most ([its record](../validation/2026-11-22-guide-replay-qhy5iii678m-rig2/README.md)) |
| **G4** | The `guider` service as [`guider.md`](../services/guider.md) specifies it: Alpaca clients, the HTTP API, the guide log, the setup page, packaging. It serves the contract `crates/rp-guider` holds since G6, and where building it changes that contract, `rp` changes with it. Eight pull requests, in this order. **The simulator's devices:** [`guide-simulator-alpaca`](../crates/guide-simulator-alpaca.md), `guide-simulator`'s mount and sky behind an Alpaca Camera and an Alpaca Telescope on the wall clock, which the service's scenarios script. **The service:** `services/guider` with its configuration validated at load, its lifecycle and state directory, its device clients and their poll, `GET /api/v1/devices`, `/health` and `guider doctor`. **The loop:** the camera loop and the sensor handling, `loop`, `star/reselect` and `guiding/stop`, the requests' turns, the metrics window, `stats` and `frame/latest`, and the camera's stop reasons. **The calibration:** the run states, the mount check and the pulses, `calibrate` and the calibration store, and the mount's stop reasons. **Guiding:** `guiding/start`, pause and resume, dither and its abort, settling, the statistics and the running totals, and the runaway and calibration stops. **The guide log**, in PHD2's format, tested against the logs G0 kept. **The setup page and the workspace:** the setup page, the packages and their units on the three platforms, doctor's catalog and its client-target joins, sentinel's list and the other hand-kept lists. **`rp` against the service:** a feature of `rp`'s runs its guiding tools, its check of the guider's devices, its watch, the guide camera guard, a frame's guiding block and the safety stop against the service and the simulator's devices. The double keeps the guiding scenarios that script what the service cannot be made to give on demand: a refusal of each code, a guider that stops answering or times out, the requests it received and the figures it settles with | BDD green against the simulator's devices, and `rp`'s feature against the service green beside its scenarios against the double; ConformU is not applicable | Merged 2026-11-28. The simulator's devices merged 2026-11-24: [stage G4, the simulator's devices](https://github.com/rusty-photon/rusty-photon/pull/1880). The service merged 2026-11-25: [stage G4, the service](https://github.com/rusty-photon/rusty-photon/pull/1887). The loop merged 2026-11-25: [stage G4, the loop](https://github.com/rusty-photon/rusty-photon/pull/1896). The calibration merged 2026-11-25: [stage G4, the calibration](https://github.com/rusty-photon/rusty-photon/pull/1899). Guiding merged 2026-11-26: [stage G4, guiding](https://github.com/rusty-photon/rusty-photon/pull/1905). The guide log merged 2026-11-26: [stage G4, the guide log](https://github.com/rusty-photon/rusty-photon/pull/1914). The setup page and the workspace merged 2026-11-27: [stage G4, the setup page and the workspace](https://github.com/rusty-photon/rusty-photon/pull/1934). `rp` against the service merged 2026-11-28: [stage G4, `rp` against the service](https://github.com/rusty-photon/rusty-photon/pull/1937) |
| **G5** | pier1 on sky under `rp`: calibrate, guide and dither, in the evaluation sessions and the unattended night of § 9. With the guider guiding, the guide-focus walk that [focus-model.md](focus-model.md) § S7 parks here (owner, 2026-11-22): `focus_train {shared: true}` on the guiding train sweeps the EAF through the main camera, then the Scops OAG's focuser by `rp`'s guide-metric `auto_focus`. pier1's guiding train first gains the `auto_focus` block that sweep reads. Five steps, in this order, each recorded under [`docs/validation/`](../validation/README.md) with any fix it turns up. **Bring-up:** a build later than G4 on pier1, with what the later steps need that pier1 lacks ([focus-model.md](focus-model.md) § S7 lists what it read from pier1): the `guider` package installed with its devices, the QHY5III715C under `qhy-camera` and the GTi, and the guiding train's focal length; `session-runner` installed, for the unattended night; `focus-model` installed, registered in `rp.json`'s `plugins` and configured for both trains, with a sweep it can size for the main train (the train's `aperture_mm` and the EAF's `microns_per_step`, or `step_size` and `half_width` in `focus-model.json`). `rp`'s guiding block points at the guider in place of `phd2-guider`, the `phd2-guider` package, which no build later than G6 carries, is removed with its units, and the guiding train gains its `auto_focus` block. `guider doctor` passes and `rp`'s check of the guider's devices passes. **First light:** a calibration and guiding on sky, settling `mount.min_pulse` and the control values [`guider.md`](../services/guider.md) leaves to this stage. **The evaluation session** of § 9, with a dither and its settle. **The unattended night** of § 9. **The guide-focus walk** | The acceptance list in § 9, and the walk returns both steps, the EAF's capture sweep and the OAG focuser's guide-metric sweep | In progress |
| **G7a** | rig2's mount over Alpaca: ASCOM Remote Server in front of the Astro-Physics V2 ASCOM driver, which reaches the Mach 1 GTO through APCC Pro, the two programs started in order in the owner's desktop session at sign-in; and a mount entry for it in `rp` on rig2 | The mount answers over Alpaca with the owner's session signed in, and `rp` on rig2 binds it (decision 13) | In progress. Both hold, checked on 2026-11-27 ([record](../validation/2026-11-27-mount-bridge-mach1-gto-rig2/README.md)) with the sign-in task run by hand in the owner's session, from a stopped chain; its trigger first fires at the owner's next sign-in. The parked mount stayed parked through the connects. The driver's connect, which Remote Server makes at every start, has not been watched on an unparked mount that is not tracking, and that case is held for the owner (project tenet 3). Remote Server does not run with nobody logged on |
| **G7** | rig2 on sky under `rp`, the guider running as a Windows service and its mount served through G7a's bridge in the owner's signed-in session | The acceptance list in § 9 | Not started |

**The guide camera changes hands on rig2.** PHD2 opened the guide
camera through the vendor SDK built into it, and on rig2 the camera was
kept out of `qhy-camera` by being left off that driver's `usb_devices`
list. G1 put it under `qhy-camera` on 2026-11-17, at device 1, and its
record lists the steps. On pier1 the camera is under `qhy-camera`
already.

**What G0 changes.** Three of G0's findings reach later stages, and
the owner has settled what each of them asked.

1. **On pier1 there is no PHD2 guiding to measure against**, and the
   owner settled on 2026-11-14 that none is needed: "We don't need
   guiding history and comparisons against phd2." PHD2 on pier1 has no
   profile with the mount in it (§ Where the rigs stand), and it gets
   none. G3 replays frames recorded without PHD2, G5 alternates with
   nothing, rig2's PHD2 is not brought back to compare with, and the
   acceptance in § 9 is on the images.
2. **G7 needs rig2's mount served over Alpaca, and known to `rp`.** The
   guider is an Alpaca client, and rig2's mount is reached through a
   COM driver. Stage G7a serves it as an Alpaca Telescope through a
   bridge in front of the vendor's ASCOM driver, in the owner's desktop
   session, and gives `rp` on rig2 its mount (decision 13). A driver of
   this workspace for Astro-Physics mounts is what serves the mount
   with nobody logged on; the owner means to build one, and no stage
   holds it.
3. **The switch at G6 takes guiding from neither rig** (decision 11).
   rig2 does not guide under `rp`, and no on-sky guiding under `rp` is
   on record for pier1. pier1's `rp` configuration points guiding at
   `phd2-guider`, so the first build deployed there after G6 comes
   with a change to that configuration.

**What G1 found.** Both rigs were measured on 2026-11-17, with
nothing guiding.

1. **The frame path passes on both guide cameras through the standard
   one-shot exposure (F1), so no driver fix comes first.** Over an hour
   of 2 s exposures, full frame binned 2 × 2, back to back through
   `qhy-camera`'s production endpoint on each rig's loopback, a client
   had the pixels 507 ms after the exposure ended on pier1's
   QHY5III715C (median; 546 ms at most over 1435 frames) and 489 ms on
   rig2's QHY5III678M (568 ms at most over 1446), with no failure and
   no drift. About 460 ms of it passes before `ImageReady`: the SDK's
   readout, the same unbinned and 264–305 ms for a 512 × 512 subframe.
   The download takes 30–70 ms binned and 184 ms for the 715C's 17 MB
   full frame. The 4 s single shots of the earlier conformance runs did
   not recur: ConformU saw `ImageReady` 2.51 and 2.53 s after
   `StartExposure`. The records:
   [QHY5III715C](../validation/2026-11-17-qhy-camera-qhy5iii715c-linux/README.md),
   [QHY5III678M](../validation/2026-11-17-qhy-camera-qhy5iii678m-windows/README.md).
   rig2's guide camera is under `qhy-camera` since, at device 1, and its
   record lists the steps that put it there. The hour ran on frames the
   SDK binned; the guider reads pier1's colour sensor unbinned and sums
   its 2 × 2 cells itself
   ([guider.md § Sensor handling](../services/guider.md#sensor-handling)).
   That path holds for an hour too: on 2026-11-18 the same script, on
   the same endpoint, looped the 715C's full frame unbinned for an hour,
   1370 frames with no failure, and had the pixels 623 ms after the
   exposure ended (median; 664 ms at most), 184 ms of it the 17 MB
   download, with the host 3 % busy
   ([record](../validation/2026-11-18-qhy-camera-qhy5iii715c-linux/README.md)).
2. **The kernels keep up on a Pi 5.** On synthetic guide frames
   (`cargo run --release -p rp-image-analysis --example kernel_timing`, a static
   build, on the project's Pi 5 runner), the per-frame path, the
   background and `measure_listed` at 30 listed stars, takes 62–67 ms
   on a binned frame (3 % of a 2 s cadence) and 266–272 ms on a full
   frame (14 %). Star selection, once, takes 0.16 s binned and 0.67 s
   full. The whole-frame `estimate_background` is 99 % of the per-frame
   path, and `measure_listed` under 1 ms; the per-frame path needs only
   the frame-wide σ from it, which the guider's core takes from a sample
   of at least 50,000 pixels
   ([guider.md § Star selection and measurement](../services/guider.md#star-selection-and-measurement)).
3. **The GTi's short pulses are steps, not proportions, below 100 ms**
   ([star-adventurer-gti.md § PulseGuide lifecycle](../services/star-adventurer-gti.md#pulseguide-lifecycle),
   at the park pose). East delivers its expected travel from 100 ms; a
   shorter East pulse moves about 1.4 ticks (0.5″) whatever its length.
   The board's West edge steps at that pose, 3.1 ticks, exceed the
   trim, so a West pulse up to 100 ms moves about 3.3 ticks (1.2″).
   Dec moves in whole counts of 0.45″, one per 59 ms at the guide
   rate, and the last count of a pulse of 100 ms or less may arrive up
   to 1.9 s late.
   `PulseGuide` returns in 3–11 ms.

Later, each behind a measurement: subframes, full-frame correlation,
predicted calibration, a tuning assistant, predictive PEC.

**Effort.** No guider project publishes person-hours. The evidence is
indirect: Ekos' author reports a simulated guider "after a couple of
weeks" in 2012, with multi-star guiding eight years later; INDIGO's
agent took 22 months from first commit to multi-star and seven years to
PHD2-style controllers; CCDciel's core is under 3,000 lines. An
estimate for this plan, in production lines of Rust: 3,000–5,000 for
the core, 1,000–2,000 for the simulator, 1,000–1,500 of new image code
beside the kernels that move (about 1,850 production lines, 3,800 with
their in-file tests), and a service about the
size of `phd2-guider` without its PHD2 client and facade (roughly
3,500–5,000), with tests at the house ratio on top. That is the same
order as the PHD2 support it replaces. The code is the smaller cost;
clear nights are the scarce one, which is what G2 and G3 are for.

## Risks

1. **Guiding quality.** A new guider guides worse than PHD2 for some
   time. The one comparison found, a 2022 forum thread on Ekos'
   internal guider, has users reporting about 1″ RMS against 0.3–0.6″
   with PHD2 on the same rigs
   (<https://forum.indilib.org/forum/ekos/12199-comparing-phd2-and-internal-guider.html>).
   The acceptance in § 9 asks for good images and not for PHD2's
   figures.
2. **Calibration in the field.** It is the part with the most hidden
   knowledge and the least that a simulator can prove.
3. **Frame cadence.** The Alpaca path may carry overhead that only
   driver work removes. G1 exists to find out first.
4. **Night time.** Every night spent on the guider is a night not
   imaging (tenet 1). The simulator and the replay are the mitigation,
   and the night the acceptance asks for is a night of imaging.
5. **The GTi.** It has no encoder, quantised pulses and unmeasured Dec
   backlash. A poor first result may be the mount, and no stage has
   PHD2 guide the GTi to tell the two apart. G1's pulse timings and
   the simulator are what the plan has for that.
6. **A second commander of the mount.** Mitigated by the pre-pulse
   checks, bounded authority and idle-after-start.
7. **Scope.** Predictive PEC, an assistant and a live view are each
   tempting and none is needed to guide.
8. **No fallback in the tree.** G6 runs first. From then on PHD2
   support exists only in the repository's history, and bringing it
   back is a revert of G6's pull requests. Until the guider passes G5,
   or that revert lands, no rig guides under a current build of `rp`.
   G0 found no on-sky guiding under `rp` on record on either rig
   (§ Where the rigs stand), so the rigs lose none they have.
9. **Work already done for PHD2.** The Linux half of the restart route
   is merged, and it goes with PHD2 at G6 (decision 12). The Windows
   keeper is not built (decision 4).

## Decisions for the owner

The owner settled decisions 1, 2, 4 and 7 on 2026-11-12, on this study.
[ADR-024](../decisions/024-native-guider-as-a-standalone-alpaca-client-service.md)
records them with their reasons. Decisions 11 and 12 are new: they
follow from decision 7 going further than the study recommended. The
owner settled decisions 6 and 12 on 2026-11-14, on what G0 found, and
decisions 3, 5 and 11 the same day. Decision 13 is new with G0 and was
settled with them. Decision 14 arose while G6's code was written, and
the owner settled it on 2026-11-15. Decision 15 arose when G3 started,
and the owner settled it on 2026-11-22. The owner revised decision 13
on 2026-11-27, on what G7a found.

1. **Build a native guider?** Or stay with PHD2 (E0, E1), or adopt
   INDIGO's agent (E3). **Settled: a native guider is built.**
2. **Placement.** A standalone service (recommended), inside `rp`, or
   a tool provider; and an ADR extending `rp`'s sorting rule to
   long-running control loops. **Settled: a standalone service that is
   an Alpaca client (L1).** The owner weighed the tool provider (L3)
   and settled on the standalone service the same day. ADR-024 extends
   the sorting rule.
3. **Clean implementation or port**, and the policy on porting
   BSD-licensed source with attribution. **Settled on 2026-11-14: a
   clean implementation, and a port of the one routine where a stage
   shows the need.** The design is written from published descriptions
   and ports nothing (P1). Where G3 or G5 shows a routine to be wrong
   in a way a port would fix, that routine is ported from PHD2 (P2) or
   from INDIGO's agent (P3) under the obligations § 10 lists, and the
   pull request says so. The owner is not asked again for each
   routine.
4. **The Windows session keeper:** hold it, or build it for PHD2 in the
   meantime. **Settled: held.** With decision 7 it is not built.
5. **Ecosystem surfaces:** the guide log from the start (recommended);
   the PHD2-compatible event server now, later or never. **Settled:**
   the guide log is in the first version (ADR-024, decision 8), and
   the event server is not planned, as the owner settled on
   2026-11-14. `rp` drives the guider over the guider's own HTTP API.
   The server can still be built if a need for it shows.
6. **The first rig and the acceptance margin** against PHD2.
   **Settled: no margin against PHD2**, in the owner's words: "We
   don't need guiding history and comparisons against phd2. We really
   only need guiding that is good enough to get us good images." The
   acceptance is on the main camera's subs (§ 9). The staging takes
   pier1 first, where the mount driver is ours. rig2's 2025 figure, a
   median RMS of 0.69″ over 55 runs (§ Where the rigs stand), records
   how PHD2 guided there, and nothing is accepted or refused against
   it.
7. **PHD2 afterwards:** retired, or kept as a supported second backend.
   **Settled: dropped**, in the owner's words: "Abandon phd2 we can
   always pick that back up if the native approach fails." There is no
   second backend and no selector. The fallback is the repository's
   history.
8. **Names and port:** `guider` on 11129, `rp-guiding`,
   `rp-image-analysis`. **Used as proposed** by ADR-024 and the design,
   with `guide-simulator` for the simulator crate. The owner can still
   rename them while nothing is built.
9. **Guide cameras:** validate the QHY5III715C and QHY5III678M under
   `qhy-camera` as part of G1, and how a colour guide camera is
   handled. **In the design:** an `RGGB` frame is summed 2 × 2 in
   software into a luminance frame
   ([guider.md § Sensor handling](../services/guider.md#sensor-handling)).
   G1 validates both cameras and may revise that.
10. **Try PHD2's development build on pier1** (E1) as a low-cost
    experiment while the native guider is built. **Not pursued**, since
    PHD2 is dropped.
11. **When `rp` switches.** With no second backend, `rp` speaks one
    contract, and the switch is one stage for every rig. **Settled on
    2026-11-14: first, before the guider is built**, in the owner's
    words: "Right away, let's drop phd2 for now. We do not need to be
    backwards compatible with this change, we have no users yet." G6
    therefore runs right after G0. PHD2's pieces leave the tree, and
    `rp`'s guiding tools speak the native contract as
    [`guider.md`](../services/guider.md) designs it, tested against a
    test double of it. Nothing answers that contract on a rig until the
    service of G4 runs there, and where building the service changes
    the contract, `rp` follows. G0 established on 2026-11-13 that the
    switch takes guiding from neither rig: rig2 has no guider under
    `rp`, with no mount entry, no guiding entry and no `phd2-guider`,
    and no on-sky guiding under `rp` is on record for pier1
    (§ Where the rigs stand).
12. **What "dropped" means before G6.** **Settled with decision 6:
    PHD2 is no measuring reference.** G0 recorded how it guides, and
    five of its guide logs stay as samples of the format the guider's
    own log is written in. G3 takes no frames or logs from PHD2, and G5
    does not alternate with it. No new work is started for PHD2, and
    no stage after G0 needs it installed on a rig. The Linux half of
    the restart route is not held (ADR-024, decision 9): it is merged,
    and it goes with PHD2. PHD2's pieces leave the tree at G6, which
    decision 11 puts first.
13. **rig2's mount over Alpaca.** The guider is an Alpaca client, and
    rig2's mount is reached through a COM driver (§ Staging, "What G0
    changes"). **Settled on 2026-11-14: a bridge first.** Stage G7a
    puts an Alpaca bridge in front of the vendor's ASCOM driver on
    rig2. **Revised on 2026-11-27, on what G7a found: the bridge runs
    in the owner's desktop session.** The bridge is ASCOM Remote
    Server, which the ASCOM Platform installs, and it does not run
    with nobody logged on. A scheduled task that runs it under the
    owner's account with nobody logged on gives it an administrator's
    token, and Remote Server's warning that it runs as an administrator
    is a message box, which throws there. With a
    basic user's token the driver's COM server does not complete its
    activation. So Remote Server and APCC Pro, through which the
    driver reaches the mount, start in the owner's session at sign-in,
    and G7a passes when the mount answers over Alpaca with that session
    signed in and `rp` on rig2 binds it. Serving the mount with nobody
    logged on is for a driver of this workspace for Astro-Physics
    mounts, which the owner means to build and no stage holds.
14. **Who keeps `rp`'s captures through the guide camera and the
    guider's runs apart.** Both are Alpaca clients of the guide camera
    through the one driver that serves it, and the camera has one
    exposure in flight, one settings cache, one image slot and one
    abort for all of them (§ 7). ADR-024 gave the guider a camera lease
    that `rp` takes. The owner weighed three ways: the guider serves a
    lease; the guider is the guide camera's only user and `rp` reads
    its frames; or `rp` arbitrates alone. **Settled on 2026-11-15: `rp`
    arbitrates alone**, as the simplest of the three and the one that
    keeps the guider's API smallest. The guider serves no camera lease.
    `rp`'s captures through the guide camera and a guiding train's
    train lease go ahead only with the guider read `idle`, and they and
    a start of guiding refuse each other at once, in `rp`'s memory
    ([rp.md § The guide camera guard](../services/rp.md#the-guide-camera-guard)).
    The owner accepted what that does not cover: a client of the
    guider's API other than `rp` can still start it while `rp`
    exposes, and a restart of `rp` forgets what it held.
    [ADR-026](../decisions/026-rp-keeps-its-guide-camera-captures-and-the-guider-apart.md) records the decision and supersedes ADR-024's
    decision 6.
15. **Where G3 records.** On pier1 the workspace drives the mount
    itself. rig2 gets more clear nights and is the rig G7 guides, but
    nothing of the workspace reaches its mount before G7a. **Settled on
    2026-11-22: G3 records on rig2.** The owner points the mount at a
    field near the meridian and starts it tracking, and the recorder
    reads the guide camera alone. G7a stays where the table puts it.
    The owner settled two more points the same day:
    - frames cropped from the recording are checked in, and a test
      replays them in CI;
    - G3's tolerance is stated before the recording, as
      [`guide-replay` § The report](../crates/guide-replay.md#the-report)
      states it.

## Not verified

- What the Device Hub and JustAHub hub drivers in rig2's ASCOM registry
  serve, and which application drove PHD2 through its event server
  there in 2025. Which mount rig2 has and how its ASCOM driver reaches
  it, which G0 left open, G7a found on 2026-11-27 (§ Staging).
- Whether pier1's guide camera has an ST4 cable to the GTi, and whether
  PHD2 ran on pier1 before the three sessions its logs hold.
- Sustained guide cadence through any camera service; the cause of the
  extra 2 s on QHY exposures; kernel timing on guide-sized frames and
  on the Pi 5. G1 measured the cadence and the kernels on 2026-11-17,
  and the extra 2 s did not recur (§ Staging, "What G1 found").
- The GTi's Dec backlash and its accuracy on pulses shorter than a
  second. G1 measured the accuracy at the park pose on 2026-11-17; the
  backlash does not show in the motor's counts.
- Centroid noise on real guide frames, with either rig's camera.
- Whether PHD2's Alpaca client supports HTTPS and Basic Auth (E1).
- INDIGO as a Windows service with nobody logged on, and any guiding of
  physical devices through `indigo_system_alpaca`.
- Any controlled measurement of the gain from multi-star guiding or
  from predictive PEC on sky.
- Whether a guider interface was ever proposed to ASCOM: the developer
  forum's search could not be read.
- Capabilities of the Rust crates named here beyond their published
  descriptions. That no Rust autoguider exists is bounded by the search
  terms used.
- The effort figures are estimates. PHD2's line counts are from a
  shallow clone at this study's date.
- Quotations from web pages passed through a text-extraction step and
  were not re-read against the raw pages.

## References

**PHD2.** Source and licence: <https://github.com/OpenPHDGuiding/phd2>.
Headless request: <https://github.com/OpenPHDGuiding/phd2/issues/683>.
Alpaca client: <https://github.com/OpenPHDGuiding/phd2/pull/1454>.
Event server:
<https://github.com/OpenPHDGuiding/phd2/wiki/EventMonitoring>. Guide
log: <https://github.com/OpenPHDGuiding/phd2/wiki/PHD2GuideLog>. Manual:
<https://openphdguiding.org/man-dev/>. Best practices:
<https://openphdguiding.org/PHD2_BestPractices_2019-12.pdf>. Log
analysis: <https://openphdguiding.org/Analyzing_PHD2_Guide_Logs.pdf>.
Log viewer: <https://github.com/agalasso/phdlogview>.

**Other guiders.** INDIGO: <https://github.com/indigo-astronomy/indigo>
(`indigo_drivers/agent_guider`, `indigo_drivers/system_alpaca`),
<https://www.indigo-astronomy.org/downloads.html>. KStars / Ekos:
<https://invent.kde.org/education/kstars>,
<https://kstars-docs.kde.org/en/user_manual/ekos-guide.html>,
<https://knro.blogspot.com/2012/09/autoguiding-in-kstars.html>. CCDciel:
<https://www.ap-i.net/ccdciel/en/documentation/internal_guider>.
N.I.N.A. guider plugins:
<https://github.com/isbeorn/nina.plugin.template/blob/master/docs/services-and-equipment.md>,
<https://github.com/aduffeck/pins-guider>. Celestron CPWI's PHD2 server:
<https://www.celestron.com/products/starsense-autoguider>.

**ASCOM.** Telescope and Camera `PulseGuide`:
<https://ascom-standards.org/newdocs/telescope.html#Telescope.PulseGuide>,
<https://ascom-standards.org/newdocs/camera.html#Camera.PulseGuide>,
<https://ascom-standards.org/newdocs/pulseguide-faq.html>. Asynchronous
methods: <https://ascom-standards.org/newdocs/relnotes.html>.

**Methods.** Donuts: McCormac et al. 2013, PASP 125, 548,
<https://arxiv.org/abs/1304.2405>; on NGTS,
<https://arxiv.org/abs/1710.11100>; code,
<https://github.com/jmccormac01/Donuts>. Gaussian-process predictive
PEC: Klenske et al. 2016, <https://doi.org/10.1109/TCST.2015.2420629>.
Plate-solved guiding at SDSS-V: <https://github.com/sdss/cherno>.

**In this repository.** [guider.md](../services/guider.md) (the
design);
[ADR-024](../decisions/024-native-guider-as-a-standalone-alpaca-client-service.md)
(the decision); [rp.md](../services/rp.md) § Tenets,
§ Component Categories, § Mount Motion Gate, § Guider Service, § Guide
Focus Watch; [phd2-guider.md](https://github.com/rusty-photon/rusty-photon/blob/865fe440e74ebc305269774bac4d9b9ac9da44f1/docs/services/phd2-guider.md) § HTTP Service
Mode, § PHD2 under the service manager, § Camera facade;
[star-adventurer-gti.md](../services/star-adventurer-gti.md)
§ PulseGuide lifecycle; [zwo-camera.md](../services/zwo-camera.md)
§ Pulse guiding; [optical-trains.md](optical-trains.md) § Guiding
integration; [daily-power-cycle.md](daily-power-cycle.md) decision 10;
`crates/rp-guider/src/lib.rs` (the contract);
`crates/rp-image-analysis/` (the kernels).

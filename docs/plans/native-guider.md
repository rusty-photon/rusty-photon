# Native guider — options for guiding without PHD2

**Date:** 2026-11-12
**Parent design docs:** [`docs/workspace.md`](../workspace.md#project-tenets) (project tenets), [`docs/services/rp.md`](../services/rp.md), [`docs/services/phd2-guider.md`](../services/phd2-guider.md), [`docs/plans/optical-trains.md`](optical-trains.md), [`docs/plans/daily-power-cycle.md`](daily-power-cycle.md)

> **Status:** options study with a recommended path. Nothing here is
> committed: § Staging is a proposal, and § Decisions for the owner lists
> what has to be settled before any of it becomes a plan with phases.

## Goal

rusty-photon guides through PHD2, wrapped by
[`phd2-guider`](../services/phd2-guider.md). PHD2 is a desktop
application, and most of what the wrapper, the packaging and the
[daily power cycle plan](daily-power-cycle.md) carry for guiding exists
to make a desktop application behave like an unattended service. This
study asks whether rusty-photon should guide with a service of its own
instead, what shapes that service can take, how each fits the Alpaca
ecosystem and rusty-photon's own architecture, and what it costs.

The recommendation in brief:

1. **Build a native guider**: a pure-Rust core library (frames in,
   corrections out) hosted by a headless service. PHD2 stays selectable
   beside it until the native guider has guided real nights on both
   rigs.
2. **The guider is an Alpaca application, not an Alpaca device.** It is
   a client of one Alpaca Camera and one Alpaca Telescope. Alpaca has no
   guider device type, and none of the software surveyed here exposes a
   guiding process as an Alpaca device.
3. **It is a standalone service, not a part of `rp`.** It serves the
   HTTP contract `rp` already consumes from `phd2-guider`, with the PHD2
   vocabulary taken out. `rp`'s design names that seam: "Swapping in a
   different guiding backend requires only a different guider service
   that implements the same HTTP endpoints"
   ([rp.md § Guider Service](../services/rp.md#guider-service)).
4. **Start with the loop every working guider runs**: multi-star
   centroids with outlier rejection, a step calibration, a proportional
   response with hysteresis, a minimum move and a pulse cap. Predictive
   and assisted pieces come later, each against a measurement.
5. **Write PHD2-format guide logs from the first version**, so the
   existing log viewers read them. A PHD2-compatible event server is a
   later, optional surface.
6. **Build the simulator before going on sky**, so that nights confirm
   behaviour instead of debugging it (tenet 1).
7. **Hold the Windows session keeper** (decision 10's Windows half)
   until this is decided. It is the largest piece of unbuilt work that
   exists only for PHD2.

What this study could not establish is in § Not verified. The two gaps
that matter most: no sustained guide-cadence measurement exists through
any camera driver, and no on-sky guiding under `rp` is on record with
any guider.

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
   ([phd2-guider.md § Camera facade](../services/phd2-guider.md#camera-facade-camera)).

### What rusty-photon carries because of them

Line counts are production lines on `main` at this study's date, with
in-file test modules excluded.

| What the stack carries | The PHD2 fact behind it | State |
|---|---|---|
| The PHD2 client in `phd2-guider`: JSON-RPC, event parsing, process start and stop, reconnect. 3,128 lines in files that hold nothing else | PHD2 speaks its own protocol and is a separate program | Built |
| The [Camera facade](../services/phd2-guider.md#camera-facade-camera): an Alpaca Camera that takes single frames through PHD2, with a capture slot and a sweep lease. 2,486 lines in its three files, 46 of the service's 85 BDD scenarios, a second port bound to loopback only | PHD2 holds the guide camera, so a focus sweep cannot use the vendor driver | Built. No gain, offset, binning or pulse guiding through it; it works only while PHD2 is stopped |
| A mock PHD2 (939 lines) and 4,670 lines of integration tests against it | CI cannot run a desktop application | Built. The mock is the only PHD2 the tests see; its `save_image` reply once had a shape real PHD2 never sends, and the client that matched it passed CI |
| Two ways to focus the guide camera: `rp`'s guide-train `auto_focus`, which reads HFD from PHD2's guide-step events and so needs an active guide loop, and focus-model's `focus_train` through the facade, which needs PHD2 stopped | HFD only while guiding; the camera is PHD2's | Built. Neither is validated on sky |
| A desktop session with nobody logged on: on Linux a virtual display unit; on Windows a dedicated account whose session `phd2-guider` keeps alive through an RDP connection to the host itself; on macOS no unattended path ([decision 10](daily-power-cycle.md#decisions)) | No headless mode | Linux half in review at this study's date; Windows half designed, not built |
| A restart route (`POST /api/v1/phd2/restart`, `rp`'s `restart_guider`) and a restart at every dusk | PHD2's QHY support does not look for cameras again after finding none; the full-pause wedge | Same as the row above |
| PHD2 built from source on each Debian or Raspberry Pi OS host | Neither distribution packages it | Per rig, by hand |
| A profile made by hand in the wizard on each rig, at risk until PHD2 next exits cleanly | GUI configuration | Per rig, by hand |
| The guide camera kept out of its vendor driver's device list | SDK ownership | Config on each rig |
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

- **pier1** is a Raspberry Pi 5 with a Star Adventurer GTi on the
  [`star-adventurer-gti`](../services/star-adventurer-gti.md) driver.
  Its guide camera, a QHY5III715C (colour), sits on an off-axis guider
  with a motorised helical focuser
  ([light path](optical-trains.md#goal)). PHD2 2.6.14 is built from
  source there.
- **rig2** is an Intel NUC on Windows 11 Pro. A QHY5III678M is held out
  of `qhy-camera`'s device list as PHD2's guide camera.
- **No on-sky guiding under `rp` is on record.** The validation records
  that involve PHD2 are facade runs and one indoor run of the guiding
  train with no stars.
- **How PHD2 drives each mount is not recorded.** The power-cycle plan
  and `rp`'s design say PHD2 reaches the mount "through Alpaca", which
  only a development build of PHD2 does. The one hardware profile on
  record for pier1 has no mount in it.

So a native guider does not displace a tuned, working guiding setup.
The costs in the table above have been paid ahead of the benefit.

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
  and SharpCap all drive PHD2 through it. N.I.N.A. uses about ten of
  its 48 documented methods. Celestron's CPWI serves a "PHD2 Server" so
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
| **L3** | A tool provider or orchestrator | Through `rp`'s MCP tools | Uses `rp`'s leases, events and safety as they are. [`polar-align`](../services/polar-align.md) is the precedent for a first-party loop driven through `rp`'s tools | Needs new `rp` tools for a guide pulse and a subframe capture. Every frame crosses two HTTP hops. Stops with `rp`, like L2 |
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
  sends the mount pulses and nothing else. `rp` keeps deciding who has
  the guide camera, as the facade's sweep lease does today.

A variant of L1 sends frames directly and pulses through `rp`, so that
`rp` stays the mount's only commander. It puts `rp` on the path of
every correction and stops guiding when `rp` restarts, for no gain the
pre-pulse checks in § Recommended shape do not already give.

**Recommendation: L1**, recorded as an ADR that extends the sorting
rule to first-party long-running control loops. Because the core is a
library, the placement can be revisited without rewriting the guider.

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

`rp` already has the single-frame kernels: `detect_stars`,
`fit_2d_gaussian`, `measure_listed`, `estimate_background` and the HFR
and SNR measures, generic over the pixel type, in
`services/rp/src/imaging/analysis/`. What does not exist anywhere in
the workspace: any frame-to-frame shift measurement, multi-star
matching, dark subtraction or a persistent defect map. Detection copies
the frame to `f64` and filters it, and no timing exists for a
guide-sized frame or for the Pi 5; the per-frame path should detect
once at star selection and afterwards measure only at the listed
positions.

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
itself while it loops, sets region, binning and gain on every exposure
instead of assuming them, and gives the camera up when stopped. `rp`
decides who has it, as it does today through the train lease.

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
   it recover" for thousands of mount and sky conditions. None of it
   exists: no simulator in the workspace models drift, periodic error
   or backlash, and none renders stars displaced by sub-pixel amounts.
   PHD2's own simulator physics is about 700 lines.
3. **Replay of recorded frames.** PHD2 can save the frame it holds, and
   its guide log gives its own measured offset for each frame. Frames
   and logs collected on the rigs while PHD2 guides are labelled
   fixtures: the native measurement runs over the same frames and the
   two are compared. No guide-camera frame is checked in today.
4. **The service, under BDD**, against simulator-backed Alpaca Camera
   and Telescope devices, with the shared test infrastructure gaining a
   Telescope stub.
5. **On sky**, pier1 first because the mount driver is ours, then rig2.
   Native and PHD2 alternate in blocks within one night on the same
   target, so the comparison shares seeing and mount.

Proposed acceptance: RMS error within a stated margin of PHD2's in the
same night; no run-away under any simulator condition; recovery from a
lost star and from a passing cloud; dither settle time no worse; and a
full night unattended.

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

**Pieces.** Names are proposals.

| Piece | Kind | Holds |
|---|---|---|
| `rp-guiding` | Library crate | The core: star set, measurement and combining, calibration, controllers, dither and settle, the state machine. No I/O, no runtime, time passed in |
| `rp-image-analysis` | Library crate | The kernels now in `services/rp/src/imaging/analysis/`, plus dark and defect handling |
| `rp-guider` | Existing contract crate | The HTTP contract and its client, with backend-neutral names |
| `guider` | Service, port 11129 | The Alpaca clients, the loop, the HTTP API, the setup page, the guide log |
| A guide simulator | Test-infrastructure crate | The mount model, the star renderer, simulator-backed Alpaca devices |

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

**Changes to the contract and to `rp`.** `PhdEquipment`, `app_state` and
`phd2_unreachable` get backend-neutral names. Results carry arcseconds
always, since the pixel scale is known from config. The contract gains
calibration state and an explicit calibrate call. `GuidingConfig` gains
a backend selector so that PHD2 and the native guider are chosen per
rig. The guiding train's camera becomes an ordinary roster camera. The
motion gate's text about pulses names the guider, not PHD2.

**Joining the workspace.** A service that is not an Alpaca device joins
sentinel's `NON_ALPACA_SERVICES` list and about ten other hand-kept
lists (the doctor catalog, the packaging port table, the Bazel service
lists, the package directory, the units, the Windows installer
fragment).

## What goes away, what stays, what is lost

**Goes away, once PHD2 is retired on a rig:** PHD2 and its build from
source; its desktop session on every platform, including the Windows
keeper that is not yet built; the profile wizard; the PHD2 client, the
facade, the mock and their tests (about 5,600 production lines by whole
file, 46 facade scenarios); the device-list exception for the guide
camera; the protocol exception in `rp`'s design; the rotator bridge
backlog item; the wedge-driven restart route, which becomes an ordinary
service restart.

**Stays:** the HTTP contract and `rp`'s guider tools; RMS statistics;
the Guide Focus Watch; the train lease; `pa-scops-oag`; everything that
is hard about guiding itself.

**Lost until rebuilt:** PHD2's field maturity; the Guiding Assistant;
predictive PEC; PHD2's window as a debugging tool; help from the PHD2
community, which supports PHD2 and not logs from another guider.

**Carried for the transition:** two guiding backends, which is more
complexity than today until the second one goes.

## Staging (proposed, not committed)

| Gate | Work | Passes when |
|---|---|---|
| **G0** | Record how each rig guides today: PHD2's profile, how it reaches the mount, one guide log per rig. Take the decisions below | The "not recorded" items in § Where the rigs stand are closed |
| **G1** | Cadence spike, no guider: loop 2 s exposures through `qhy-camera` on each guide camera, full frame binned and subframe; time short pulses through the GTi driver; time the kernels on the Pi 5 | Exposure end to pixels in the client well under the exposure length, sustained for an hour. If not, the driver overhead is fixed first |
| **G2** | The kernels move to a library crate. The core and the simulator: multi-star measurement, step calibration, the two controllers, dither and settle | The loop converges and recovers across the simulator's mount and sky conditions in CI |
| **G3** | Replay: collect frames and guide logs on pier1 while PHD2 guides; compare measurements | Native and PHD2 offsets agree frame by frame within a stated tolerance |
| **G4** | The service: Alpaca clients, the contract, the guide log, the setup page, packaging; `rp`'s backend selector | BDD green against simulator devices; ConformU is not applicable |
| **G5** | pier1 on sky: calibrate, guide, dither, alternate with PHD2 | The acceptance list in § 9 |
| **G6** | rig2 on sky, as a Windows service with nobody logged on | The same |
| **G7** | The native guider becomes the default; PHD2's pieces are retired one pull request at a time | — |

Later, each behind a measurement: full-frame correlation, predicted
calibration, the event server, a tuning assistant, predictive PEC.

**Effort.** No guider project publishes person-hours. The evidence is
indirect: Ekos' author reports a simulated guider "after a couple of
weeks" in 2012, with multi-star guiding eight years later; INDIGO's
agent took 22 months from first commit to multi-star and seven years to
PHD2-style controllers; CCDciel's core is under 3,000 lines. An
estimate for this plan, in production lines of Rust: 3,000–5,000 for
the core, 1,000–2,000 for the simulator, 1,000–1,500 of new image code
beside the 3,800 lines of kernels that move, and a service about the
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
2. **Calibration in the field.** It is the part with the most hidden
   knowledge and the least that a simulator can prove.
3. **Frame cadence.** The Alpaca path may carry overhead that only
   driver work removes. G1 exists to find out first.
4. **Night time.** Every night spent on the guider is a night not
   imaging (tenet 1). The simulator, the replay and same-night
   alternation are the mitigation.
5. **The GTi.** It has no encoder, quantised pulses and unmeasured Dec
   backlash. A poor first result may be the mount; alternating with
   PHD2 on the same night separates the two.
6. **A second commander of the mount.** Mitigated by the pre-pulse
   checks, bounded authority and idle-after-start.
7. **Scope.** Predictive PEC, an assistant and a live view are each
   tempting and none is needed to guide.
8. **Two backends** until PHD2 is retired, and no fixed date for that.
9. **Work already done for PHD2.** The restart route is useful while
   PHD2 is in use. The Windows keeper is not started, and building it
   and then retiring PHD2 would be work thrown away.

## Decisions for the owner

1. **Build a native guider?** Or stay with PHD2 (E0, E1), or adopt
   INDIGO's agent (E3).
2. **Placement.** A standalone service (recommended), inside `rp`, or
   a tool provider; and an ADR extending `rp`'s sorting rule to
   long-running control loops.
3. **Clean implementation or port**, and the policy on porting
   BSD-licensed source with attribution.
4. **The Windows session keeper:** hold it, or build it for PHD2 in the
   meantime.
5. **Ecosystem surfaces:** the guide log from the start (recommended);
   the PHD2-compatible event server now, later or never.
6. **The first rig and the acceptance margin** against PHD2.
7. **PHD2 afterwards:** retired, or kept as a supported second backend.
8. **Names and port:** `guider` on 11129, `rp-guiding`,
   `rp-image-analysis`.
9. **Guide cameras:** validate the QHY5III715C and QHY5III678M under
   `qhy-camera` as part of G1, and how a colour guide camera is
   handled.
10. **Try PHD2's development build on pier1** (E1) as a low-cost
    experiment while the native guider is built.

## Not verified

- How PHD2 drives the mount on either rig, whether rig2 guides at all
  (the documents disagree), and which mount rig2 has.
- Sustained guide cadence through any camera service; the cause of the
  extra 2 s on QHY exposures; kernel timing on guide-sized frames and
  on the Pi 5.
- The GTi's Dec backlash and its accuracy on pulses shorter than a
  second.
- Centroid noise on real guide frames, with either rig's camera.
- That PHD2's frame saving keeps up with every frame at guide cadence
  (G3 depends on it).
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

**In this repository.** [rp.md](../services/rp.md) § Tenets,
§ Component Categories, § Mount Motion Gate, § Guider Service, § Guide
Focus Watch; [phd2-guider.md](../services/phd2-guider.md) § HTTP Service
Mode, § PHD2 under the service manager, § Camera facade;
[star-adventurer-gti.md](../services/star-adventurer-gti.md)
§ PulseGuide lifecycle; [zwo-camera.md](../services/zwo-camera.md)
§ Pulse guiding; [optical-trains.md](optical-trains.md) § Guiding
integration; [daily-power-cycle.md](daily-power-cycle.md) decision 10;
`crates/rp-guider/src/lib.rs` (the contract);
`services/rp/src/imaging/analysis/` (the kernels).

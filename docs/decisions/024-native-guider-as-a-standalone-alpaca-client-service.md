# ADR-024: A native guider as a standalone Alpaca client service

## Status

Accepted (2026-11-12, by the owner on the options study
[`docs/plans/native-guider.md`](../plans/native-guider.md),
[#1764](https://github.com/rusty-photon/rusty-photon/pull/1764)). The
service is designed in [`docs/services/guider.md`](../services/guider.md)
and is not yet built; the plan's § Staging orders the work.

Extends the sorting rule of
[rp.md § Component Categories](../services/rp.md#component-categories)
by one case (decision 3). Holds the Windows half of decision 10 of the
[daily power cycle plan](../plans/daily-power-cycle.md#decisions), the
session keeper (decision 9).

## Context

rusty-photon guides through PHD2, wrapped by
[`phd2-guider`](../services/phd2-guider.md). The options study sets out
what that costs and why (§ Background). In brief, four properties of
PHD2 drive the cost, and none of them is a defect in PHD2:

1. **It is a desktop application with no headless mode.** Running it on
   a rig with nobody logged on needs a virtual display on Linux and a
   dedicated account with a kept-alive RDP session on Windows; macOS has
   no unattended path.
2. **It owns its devices through its own drivers.** The guide camera is
   opened through the vendor SDK built into PHD2, so the camera's
   rusty-photon driver cannot serve it. `phd2-guider` carries a Camera
   facade (2,486 production lines, 46 of the service's 85 BDD scenarios)
   so that a focus sweep can take frames through PHD2.
3. **It is configured through its GUI.** Each rig's equipment profile is
   made by hand in a wizard.
4. **Its control surface is built for an interactive application.** It
   speaks its own JSON-RPC protocol, "the one exception to the
   Alpaca-only rule" in `rp`'s design; it has no dither abort; it reports
   the pixel scale only after a calibration; and CI can run only a mock
   of it.

Three more facts frame the decision:

- **No tuned guiding setup is displaced.** No on-sky guiding under `rp`
  is on record with any guider (the study's § Where the rigs stand). The
  costs above have been paid ahead of the benefit.
- **Alpaca has what a guider needs and no place for a guider.** Frames,
  subframes, the pixel size, pulses, guide rates, the declination, the
  pier side and the mount's tracking, slewing and park state are all
  standard Camera and Telescope members. The standard defines no guider
  device type and no way to register one. In every ecosystem the study
  surveyed, the guiding process is controlled over a protocol of its own
  and is, where Alpaca appears at all, an Alpaca **client**.
- **`rp`'s sorting rule does not cover the case.** It puts algorithms
  that are "pure Rust math" inside `rp` and makes a component an
  rp-managed service "only when they must wrap an external program".
  Its examples are tool calls that start, run and return. A guider is a
  control loop that runs for hours with device traffic of its own, and
  the rule does not say where such a thing belongs.

## Options Considered

### Option 1: Stay with PHD2

Keep the stable release as it is and build the Windows session keeper,
or move to PHD2's development build, whose Alpaca client can take the
camera and the mount from the rusty-photon drivers (the study's E0 and
E1).

- **Pros.** More than a decade of field use; defaults that work on an
  unknown mount; multi-star guiding, predictive periodic-error
  correction and the Guiding Assistant; the guide log and a community
  that reads it.
- **Cons.** The desktop session, the hand-made profile, the PHD2 client
  and its mock all stay under either build. The stable build also keeps
  the facade and still needs the keeper built. The development build
  removes the facade only if PHD2 takes its camera from the vendor
  driver, and it is no stable release.

### Option 2: INDIGO's guider agent

A headless server with a guider agent and a permissive licence (E3).

- **Pros.** No desktop session; multi-star selection, full-frame
  correlation and several controllers exist today.
- **Cons.** A second server, a second protocol and a wrapper the size of
  `phd2-guider`. To use the rusty-photon drivers it needs
  `indigo_system_alpaca`, which its README calls "Under development" and
  tested "with simulators only", without HTTPS or authentication. Every
  route from INDIGO to the rigs' devices either bypasses the drivers this
  project exists to provide or crosses that bridge.

### Option 3: A native guider inside `rp`

The guiding core as built-in code in `rp` (L2).

- **Pros.** No new service and no second Alpaca client: `rp` already
  holds both device connections and the image kernels. It is what the
  sorting rule says when read literally.
- **Cons.** A restart or crash of `rp` stops guiding in the middle of an
  exposure. `rp`'s capture path is shaped for science frames: a FITS
  file and a sidecar, synced to disk, for each frame. The guider cannot
  be used without `rp`.

### Option 4: A native guider as a tool provider or orchestrator

A separate process that takes frames and sends pulses through `rp`'s MCP
tools (L3). [`focus-model`](../services/focus-model.md) and
[`calibrator-flats`](../services/calibrator-flats.md) are first-party
tool providers and [`polar-align`](../services/polar-align.md) is a
first-party orchestrator, each a loop driven through `rp`. The owner's
first lean on 2026-11-12 was this option; the same day's clarification
settled on option 6.

- **Pros.** `rp` stays the only client that commands the mount and the
  camera, with its leases, events and safety rules as they are.
- **Cons.** The loop stops when `rp` stops, as in option 3, because
  every frame and every pulse goes through it. `rp` needs a guide-pulse
  tool and a capture that writes no file, and the
  [motion gate](../services/rp.md#mount-motion-gate), which exempts
  pulses because the guider sends them to the mount directly, needs its
  rules for pulses written anew. Latency does not decide it: with the
  services on one host a further loopback hop is small beside an
  exposure of seconds, and what the frame path costs is unmeasured under
  every option.

### Option 5: A native guider inside a camera driver

The loop in the driver process, reading frames from the SDK (L4).

- **Pros.** The shortest frame path.
- **Cons.** Guiding is tied to one vendor, a driver becomes a client of
  the mount, and the camera is again owned by its guider, which is
  PHD2's second property.

### Option 6: A native guider as a standalone service and Alpaca client (chosen)

A pure-Rust core library hosted by a headless service that is an Alpaca
client of one Camera and one Telescope and serves `rp` over HTTP (L1).

- **Pros.** Guiding survives a restart of `rp`. The loop shares no
  process with `rp`'s other work. It takes `phd2-guider`'s place at a
  seam `rp`'s design already has: a guider service that `rp` reaches
  over HTTP. The guide camera is an ordinary camera on its vendor
  driver. It runs wherever a rusty-photon service runs, with nobody
  logged on. CI can close a guide loop against simulated devices. It
  can be used without `rp`.
- **Cons.** A second Alpaca client of two devices. One more service to
  package and supervise. Its endpoints are not `phd2-guider`'s, so `rp`
  changes to speak them (decision 7). The guider is this project's
  code, and so is every failure of it: PHD2's field maturity has to be
  earned again.

## Decision

1. **rusty-photon guides with a guider of its own.** A core library
   takes frames and timestamps and gives pulse requests and statistics.
   It does no I/O, and time and randomness are passed in, so the whole
   loop runs under test in simulated time. A service hosts it.

2. **The guider is a standalone service and an Alpaca client
   application.** It is a client of one Alpaca Camera and one Alpaca
   Telescope. It is not an Alpaca device, since the standard has no
   guider type and no client would discover an invented one. It is not
   an `rp` plugin either: `rp` reaches it over the HTTP contract in
   `crates/rp-guider` and proxies its operations as built-in tools, as
   it does for `phd2-guider`.

3. **The sorting rule gains a case.** A first-party control loop is an
   rp-managed service when it runs for hours, has device traffic of its
   own and must keep running while `rp` restarts. The rule's boundary,
   "process supervision and lifecycle role, not authorship", is
   unchanged; the guider is the first component of this kind. Algorithms
   that are tool calls (auto-focus, centering) stay inside `rp`.

4. **PHD2 is dropped, with no second backend.** `rp` gets no backend
   selector and the guider no PHD2 compatibility layer. No new work
   starts for PHD2. The plan's staging keeps PHD2 guiding a rig, on the
   release that rig runs, until the native guider takes over there;
   whether PHD2 comes off the rigs sooner, and when `rp` switches, are
   open in the plan
   ([decisions 11 and 12](../plans/native-guider.md#decisions-for-the-owner)).
   When `rp` moves to the native guider's contract, `phd2-guider`, its
   Camera facade, its mock and `rp`'s PHD2-specific paths leave the
   tree. Should the native guider fail, the fallback is the repository's
   history: the last commit that carries them.

5. **The guider's authority over devices is fixed.**
   - On the Telescope it writes `PulseGuide` and nothing else.
   - On the Camera it uses the exposure members and writes the frame
     settings they need: the binning, the region and, when its
     configuration names them, the gain and the offset. It does so only
     while `rp` holds no lease on the camera (decision 6).
   - It never changes `Connected` on either device: `rp` owns device
     connections.
   - It is idle after every start. It never calibrates, loops or guides
     because it started, restarted, found a device again or had its
     configuration saved
     ([tenet 3](../workspace.md#project-tenets), no actuation on
     connect).
   - Before every pulse it reads the mount's tracking, slewing and park
     state, and sends nothing unless the mount is tracking, still and
     unparked. A failed check stops the run; the guider does not wait
     and resume.
   - Every pulse is capped, no pulse is sent while the stars are lost,
     and corrections that pile up in one direction without the error
     shrinking stop the run.

6. **The guide camera is an ordinary camera, shared through a lease the
   guider serves.** Its vendor driver serves it and it sits in `rp`'s
   roster like any camera. An Alpaca camera has one exposure in flight
   and one image slot for all its clients, so two clients have to be
   coordinated outside the standard. The guider serves a lease over
   HTTP: while `rp` holds it the guider refuses to expose, and the
   guider grants it only when idle. It takes the place of the facade's
   sweep lease.

7. **The contract is the HTTP contract in `rp-guider`, without PHD2 in
   it.** The PHD2 names become backend-neutral ones. Results carry
   arcseconds beside pixels, because the pixel scale is known from the
   camera's pixel size and a configured focal length before any
   calibration. The contract gains an explicit calibration, the
   calibration's record, a dither abort and the camera lease.
   [`guider.md` § HTTP API](../services/guider.md#http-api) specifies
   the contract; beyond the names it also changes `calibration/clear`,
   which empties the whole store and is refused in a run, and adds
   `loop`, the latest frame and the stop reason.

8. **The guider writes PHD2-format guide logs from its first version.**
   The existing log viewers read a night's guiding without change. A
   PHD2-compatible event server is a later, optional surface.

9. **The Windows session keeper is held.** It is the largest piece of
   unbuilt work that exists only for PHD2, and it is not built unless
   this decision is reversed. The Linux half of the power cycle plan's
   decision 10 is not affected: it serves a rig for as long as PHD2
   guides it.

## Consequences

- **A restart of the guider ends a guiding run.** Guiding state lives in
  PHD2 today, so a restart of `phd2-guider` costs one in-flight request.
  The native guider holds the loop itself and starts idle (decision 5),
  so an upgrade, a restart after a config save or a sentinel restart of
  the service during a run stops guiding. Nothing starts it again by
  itself: that is the decision of the guider's client, and an exposure
  that spans the restart records that guiding did not run throughout.
- **The mount has a second client that commands it.** `rp`'s motion gate
  already exempts guide pulses for a guider that sends them directly;
  that model stays. What protects the mount from a guider that keeps
  correcting after someone parks or slews is the check before every
  pulse.
- **One more first-party service joins the workspace.** It is packaged,
  supervised by sentinel as a service that is not an Alpaca device, and
  added to the doctor catalog and the other hand-kept service lists.
- **The guide camera's driver is on the path of every guide frame.**
  The cadence of one-shot exposures through an Alpaca camera service is
  unmeasured. The plan's stage G1 measures it before anything else is
  built, and driver overhead found there is fixed in the driver.
- **Focusing the guide camera needs no facade.** A focus sweep of the
  guiding train goes through `rp`'s ordinary capture path, with gain and
  binning available, and the guider reports the half-flux diameter of
  its guide stars on every frame.
- **`rp`'s design loses its protocol exception.** The guider speaks
  Alpaca to devices and HTTP to `rp`
  ([rp.md § Tenets](../services/rp.md#tenets), "Remote interfaces
  only").
- **Lost until rebuilt:** PHD2's field maturity, the Guiding Assistant,
  predictive periodic-error correction, PHD2's window as a debugging
  tool, and help from the PHD2 community, which supports PHD2 and not
  logs from another guider.
- **Night time is spent on the guider** before it guides as well as
  PHD2 does. The plan puts a closed-loop simulator and a replay of
  recorded frames ahead of the first night on sky, so that nights
  confirm behaviour and do not debug it
  ([tenet 1](../workspace.md#project-tenets)).
- **The placement can be revisited without rewriting the guider.** The
  core is a library; options 3 and 4 would host the same crate.

## References

- [`docs/plans/native-guider.md`](../plans/native-guider.md) — the
  options study: what guiding through PHD2 costs, what the Alpaca
  ecosystem offers a guider, the options and their evidence, the
  staging, the risks and what the study could not verify.
- [`docs/services/guider.md`](../services/guider.md) — the service's
  design.
- [`docs/services/phd2-guider.md`](../services/phd2-guider.md) — the
  service this one replaces, and the HTTP contract it serves today.
- [rp.md § Component Categories](../services/rp.md#component-categories),
  [§ Guider Service](../services/rp.md#guider-service),
  [§ Mount Motion Gate](../services/rp.md#mount-motion-gate) — the
  sorting rule, the seam and the pulse exemption.
- [`docs/plans/daily-power-cycle.md`](../plans/daily-power-cycle.md)
  decision 10 — PHD2 under the service manager, whose Windows half
  decision 9 holds.
- [ADR-023](023-services-configure-themselves-on-their-own-setup-pages.md)
  — the setup page the guider serves in place of PHD2's profile wizard.
- <https://github.com/OpenPHDGuiding/phd2/issues/683> — the upstream
  request for a headless PHD2, open since 2018.
- <https://github.com/OpenPHDGuiding/phd2/wiki/PHD2GuideLog> — the
  guide log format of decision 8.

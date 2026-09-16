# Device Claims + the PHD2 Guide-Camera Facade

## Goal

Two problems that look unrelated share one root: **the guide camera is the
only device in the rig that no Alpaca driver may own.**

1. **Every SDK-backed driver claims every device its SDK enumerates.**
   `qhy-camera`, `zwo-camera` and `svbony-camera` register each camera the
   vendor SDK reports. There is no way to say *"this one belongs to PHD2."*
   For QHY it is worse than an ownership question: `Sdk::new()`
   ([`crates/qhyccd-rs/src/sdk.rs`](../../crates/qhyccd-rs/src/sdk.rs)) runs
   `cfw_probe` — `OpenQHYCCD` → `InitQHYCCD` → `IsQHYCCDCFWPlugged` ×3 →
   `CloseQHYCCD` — against **every** camera the scan returns, before any
   config is consulted. `InitQHYCCD` is a full device configuration, not a
   read. Run against a camera PHD2 is streaming, it is the disruptive call.
   The probe runs at service start, again on config reload, and inside
   `qhy-camera doctor`.

2. **The guide camera is outside Alpaca, so rp cannot capture through it.**
   PHD2 owns it at the SDK level, which is why
   [`rp.md` § Guide-train sweep](../services/rp.md) makes guide-train
   autofocus a special case: it moves the focuser and reads PHD2's per-frame
   HFD instead of capturing. That variant requires an *active guide loop*,
   which is exactly the wrong precondition for "focus the guide camera
   before the night starts."

The outcome of this plan:

- **Part A — claims.** Each SDK-backed driver gains a `claims` config block
  naming which enumerated devices it registers, keyed by **USB port path —
  the only key**, resolved from a **passive** host USB scan and applied
  **before** any device-touching probe. A driver never opens a device it
  does not own. `doctor` prints the paste-ready claim for every device on
  the bus, so nobody types a port path from memory.
- **Part B — the PHD2 camera facade.** `phd2-guider` additionally serves an
  ASCOM Alpaca **Camera** device backed by PHD2's own frame capture, so the
  guide camera appears in rp's roster like any other camera and the ordinary
  `auto_focus` capture sweep works on the guiding train.

Part A makes Part B safe (the guide camera is explicitly excluded from the
vendor driver instead of accidentally skipped), and Part B makes Part A
worth doing (the excluded camera is still reachable, through PHD2).

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| C0 | This plan | Not started | |
| C1 | **Hardware spike + passive USB identity**: confirm the Windows port spelling on the real box (direct and behind a hub, across replug and reboot), then `UsbDevice` gains `port` + `serial` on all three platforms | Not started | |
| C2 | `claims` schema + `svbony-camera` — the easy case, proves schema, join and doctor output | Not started | |
| C3 | `claims` in `zwo-camera` | Not started | |
| C4 | `claims` in `qhy-camera` + `qhyccd-rs` enumerate/probe split — restores the documented enumeration-only contract | Not started | |
| C5 | `doctor --devices` setup help + `claims.resolve` / `claims.unclaimed` checks, port-based `UniqueID` fallback for serial-less cameras, stable `device_number`, `config.schema`/`config.apply` exposure | Not started | |
| C6 | `phd2-guider` Alpaca Camera facade (design doc → BDD → code), incl. the `save_image` wire-format fix | Not started | |
| C7 | rp wiring: guide camera as a train-terminal camera, capture-sweep AF on the guiding train, doc updates | Not started | |
| C8 | `ui-htmx` claims editing | Deferred | |

Order: C1 first and **blocking** — no schema commits to a port spelling
that has not been proven stable on all three platforms. C2 → C3 → C4 then
land per driver, each on its own. C5 folds in once two drivers carry
`claims`. C6 needs nothing from Part A but is only *operationally* safe
after C4. C7 needs C6.

Each phase follows
[development-workflow.md](../skills/development-workflow.md): design-doc
update first (`qhy-camera.md` / `zwo-camera.md` / `svbony-camera.md` /
`phd2-guider.md` / `rp.md`), BDD second, implementation third.

---

## Part A — Device claims

### D1. The USB port path is the only key

Surveyed against the three vendored SDK crates and the host USB layer
rather than assumed:

| Candidate key | Exists for every camera? | Readable without opening the device? | Ambiguous? |
|---|---|---|---|
| **USB port path** | **Yes** — every device on the bus has one | **Yes**, on every platform | No — one device per port |
| SDK serial | No — many cameras report none | ZWO: no (needs `ASIOpenCamera`). QHY/SVBony: yes | No, when it exists |
| Model name | Yes | Yes | Yes, with two of a kind |

Per-SDK detail for the serial route, for the record:

| SDK | Identity | Free at enumeration? |
|-----|----------|------------------------|
| QHYCCD | `GetQHYCCDId(index)` → `QHY268M-<serial>` | Yes — but the trailing field is a constant on models with no flash serial |
| ZWO ASI | `ASIGetSerialNumber`, else `ASIGetID` | No — needs `ASIOpenCamera`; older models (ASI1600) expose neither |
| SVBony | `CameraSN` in the enumeration struct | Yes |

**Decision: the USB port path is the claim key, and the only one.** Serial
fails the first column — the ASI1600 has none at all — and this is an
observatory, where a cable is seated once and stays. A cable move is a
config edit; that is an acceptable, rare cost for a key that exists for
every device, is unambiguous, and costs nothing to read.

Supporting serial or model as *alternative* keys was considered and
rejected: three ways to name one device means three code paths, three
doctor messages, three ways for two entries to disagree about the same
camera, and a config whose meaning depends on which key the author
reached for. One key, one meaning.

Serial and model do not disappear — they remain **internal join signals**
(D4) and **doctor display columns** (D5). They are simply never config
surface.

The decisive property of the port key is that **USB identity is fully
passive on every platform**: Linux reads it from sysfs, macOS from
`system_profiler`, Windows from PnP properties — the kernel cached all of
it at enumeration, so nothing is opened, claimed, or reset. A serial-keyed
claim could not have said the same: deciding which ZWO cameras to skip
would have required opening every one of them first.

### D2. The port path already exists in this repo

`rusty-photon-doctor-checks`'s `facts` module already enumerates USB
passively and cross-platform for the D4 `hardware.usb-device` check, with
**no third-party dependency**:

- **Linux** — walks `/sys/bus/usb/devices`, reading `idVendor`,
  `idProduct`, `product`. The **directory name is the port chain**
  (`1-4.2` = bus 1, root port 4, hub port 2), so capturing it is
  `entry.file_name()`. The `serial` attribute sits in the same directory,
  equally free.
- **macOS** — `system_profiler -json SPUSBDataType`, which carries
  `location_id` (a hex encoding of the port chain) per device.
- **Windows** — `Get-PnpDevice` + `Get-PnpDeviceProperty`. Today it reads
  the instance id and `DEVPKEY_Device_BusReportedDeviceDesc`; the port
  chain needs one more property. Candidates are
  `DEVPKEY_Device_LocationPaths`
  (`PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(4)#USB(2)` — a full chain, which is
  what we want) and `DEVPKEY_Device_LocationInfo` (`Port_#0004.Hub_#0003`,
  one level, needing a parent walk for nested hubs).

**C1 is a hardware spike, and it gates everything else in Part A.** Run
both properties against a real camera on the Windows box — plugged
directly, then behind a hub — and confirm the string is stable across
replug and reboot before any config schema commits to a spelling. The
Linux and macOS legs are already proven by code in this repo; Windows is
not, and a claim key that turns out to be unstable on one platform is
worse than no claim key at all.

So Part A extends `UsbDevice` with two optional fields — `port` (the key)
and `serial` (a join signal) — and reuses the existing inventory. **No new
crate dependency, no `crate_universe` repin.**

**The port string is the platform's native spelling, not a normalised
invention.** `1-4.2` on Linux, the location path on Windows, the location
id on macOS. A config file already names one specific host's hardware; it
is not portable across an OS boundary, and inventing a canonical form only
creates a second thing that can disagree with what the OS says. Doctor
prints the exact string to paste (D5), so the operator never types one.

### D3. Config shape

Added to each camera service's `Config`, next to `devices`:

```json
{
  "claims": { "mode": "all" },

  "claims": { "mode": "include", "usb_ports": ["1-4.2"] },

  "claims": { "mode": "exclude", "usb_ports": ["1-4.3"] }
}
```

That is the whole surface. `mode: "all"` is the default, so every existing
config keeps working unchanged. `usb_ports` is required for `include` and
`exclude`, rejected for `all`, and `deny_unknown_fields` applies as
everywhere else in the config tree. An `include` with an empty list
registers nothing: legal, and logged loudly — it is how an operator hands
a whole SDK to another application for a night.

The reference rig's `qhy-camera.json` becomes one line:

```json
"claims": { "mode": "exclude", "usb_ports": ["1-4.3"] }
```

A comment naming the camera is the operator's business; the config does
not carry a second name for the same device that could fall out of date.

### D4. Behaviour

1. **Claims are applied before any device-touching call.** The one rule
   that cannot be relaxed per driver. The USB inventory yields ports
   passively; the filter runs; only survivors are opened, probed or
   initialised.
2. **Resolving a claimed port to an SDK device is a join.** The USB scan
   knows `(port, vid, pid, product string, serial?)`; the SDK knows
   `(index, model, id/serial?)`. They are matched by serial when both
   sides have one, else by model when that model is unique on the bus.
   **Serial and model are join signals here, not keys** — the operator
   never writes them.
3. **An unresolvable join is reported, never guessed.** Registering the
   wrong camera is worse than registering none: a guess means the guide
   camera answers to the main camera's Alpaca device number, and the first
   symptom is a subframe from the wrong sensor at 2am. The ambiguous case
   is two identical models that both report no serial; D5 gives the
   operator a way through it.
4. **A claimed port with nothing in it** registers nothing, logs `warn!`,
   and produces a *soft* doctor finding. Never a startup failure: a
   powered-down hub is a normal Tuesday, and a driver that refuses to
   start because one of three cameras is absent is worse than one that
   starts with two.
5. **The port strengthens `UniqueID` for serial-less cameras.**
   `zwo-camera`'s `mint_identity` falls back to `noserial-{index}` when a
   camera exposes neither serial nor flash id, and its own doc comment
   names the weakness: *"two serial-less cameras of the same model
   reordered on the bus … could swap identities."* With the port in hand
   that fallback becomes `noserial-{port}`, which is stable across bus
   reordering and replug. Same change for the other two drivers'
   equivalents. Small, obviously right, and it closes a documented
   ambiguity — but it **changes `UniqueID` for affected cameras**, so it
   needs a release note and lands with C5, not silently inside a driver
   phase.
6. **`device_number` stability.** Alpaca device numbers are assigned by
   enumeration order today, so unplugging one camera renumbers the others
   and silently re-points every `cameras[].device_number` in rp's config.
   Assign them instead by the claim's position in `usb_ports` (explicit
   claims) or by sorted port (`mode: "all"`), so a device number is a
   function of the operator's declaration, not of scan order. A
   correctness fix worth doing on its own; breaking for existing
   multi-camera configs, so it needs a release note.

Note the deliberate asymmetry: **claims are port-keyed, while the
`devices` override map and the ASCOM `UniqueID` stay serial-derived.**
`UniqueID` is a shipped contract that rp and every ASCOM client stores;
re-keying it on the port would mean a device's identity changed when its
cable moved, which is exactly wrong for identity even though it is exactly
right for a claim. The two vocabularies answer different questions —
*which socket is this?* versus *which camera is this?* — and D4.5 is the
one place they meet.

### D5. Doctor as the setup tool

The operator never types a port path from memory. Every catalog camera
service's existing `doctor` subcommand grows a device listing — the
inventory it already gathers, printed as claim-ready rows:

```
$ qhy-camera doctor --devices

USB devices for this driver (VID 1618)

  Port     Model          SDK id                      Serial   Claimed
  1-4.2    QHY268M        QHY268M-b4a1f2ab3c4d5e6f    b4a1f2…  yes
  1-4.3    QHY5III715C    QHY5III715C-0000000000      —        no  (excluded)

Claim only the main camera — paste into qhy-camera.json:

    "claims": { "mode": "include", "usb_ports": ["1-4.2"] }

  ok    claims.resolve      2 devices enumerated, 1 claimed, 1 excluded
```

Model and SDK id are shown so the operator can tell which row is which
camera; neither is something they ever type.

Two checks join the per-service set, alongside `config.full-shape` and
`hardware.sdk-devices`:

| Check | Trigger |
|---|---|
| `claims.resolve` | A claimed port holds no device (`warn` — absent hardware or a moved cable), or holds one that cannot be joined to an SDK device (`fail` — the driver will register nothing for it). |
| `claims.unclaimed` | Devices on the bus that no claim covers, listed for information (`ok`) — so "why is my camera missing" answers itself. |

An automated *drift* check ("this port used to hold a QHY268M") is
deliberately **not** in scope: with the port as the only key there is
nothing in the config to compare against, and inventing a remembered-state
file to enable one check is not worth it. The listing above shows the
operator what is where; a moved cable shows up as a `claims.resolve` warn
plus an unfamiliar model in the table.

**Ambiguity has a printed procedure**, not just an error. When two
identical serial-less cameras cannot be told apart, doctor says so and
tells the operator to unplug one and re-run: the port that disappears is
the one they just unplugged. That is the whole disambiguation protocol,
and it needs no vendor cooperation.

`--devices` is read-only and **enumeration-only**, like every other
per-service check.

### D6. The contract C4 restores

`doctor.md` already states the rule for `hardware.sdk-devices`:

> **Enumeration only, never an open** — an open against a device the
> running service holds is the camera-lock class of bug, and the
> subcommand must stay safe to run by hand at any time.

`qhy-camera doctor` violates this today. Its probe path runs
`qhyccd_rs::Sdk::new()`, which opens **and initialises** every camera on
the bus for the CFW probe before any filtering can happen. C4 is therefore
not a new feature so much as making the code match a contract the design
docs already assert. Split the vendored crate's constructor:

```rust
// stage 1: identities only — ScanQHYCCD + GetQHYCCDId, nothing opened
let ids: Vec<String> = Sdk::enumerate_ids()?;

// stage 2: probe (open → init → CFW → close) only the claimed ids
let sdk = Sdk::open_claimed(&claimed)?;
```

`qhyccd-rs` stays generic — it takes the claimed set; it never learns
about rusty-photon config. The service applies `claims` between the
stages.

### D7. Per-driver work

**`svbony-camera` (C2)** — filter the `CameraInfo` list before
registering. Serial is free at enumeration, so every join resolves. Proves
the schema, the join and the doctor output on the easy case.

**`zwo-camera` (C3)** — with port-keyed claims the passive
`open_uninitialised()` is no longer part of deciding ownership at all. It
runs only for cameras already claimed, to mint their identity. An excluded
ZWO camera is now never opened, where today every camera is.

**`qhy-camera` (C4)** — the enumerate/probe split above, plus the filter.

### D8. Out of scope for Part A

- Filter wheels, focusers and rotators. The pattern generalises, but the
  motivating conflict is cameras, and `zwo-focuser`/`qhy-focuser` have no
  competing consumer today. Widen when a second consumer appears.
- Hot-plug. Claims resolve at enumeration (start / reload), not watched.
- Re-keying `devices` overrides or `UniqueID` on the port (see the note
  under D4).
- `ui-htmx` editing (C8).

---

## Part B — PHD2 as an Alpaca Camera

### D9. What PHD2 can actually deliver

Verified against PHD2's EventMonitoring wiki, not assumed:

| RPC | Delivers | Constraint |
|-----|----------|------------|
| `capture_single_frame{exposure, subframe}` | `integer(0)` — an acknowledgement, no pixels | **"guiding and looping must be stopped first"** |
| `save_image` | `{"filename": "<full path to FITS>"}` on **PHD2's** host; *"the client should remove the file when done with it"* | Whole frame, but only as a path |
| `get_star_image{size}` | `{frame, width, height, star_pos, pixels}`, base64 16-bit row-major | Only a ≥15 px cutout, and errors unless a star is selected |

So the facade is `set_exposure` + `capture_single_frame` + poll + `save_image`
+ decode FITS. There is no full-frame-over-the-wire path, and no "frame
done" event — completion is observed by the app state returning to
`Stopped`.

**Prerequisite defect:** `Phd2Client::save_image`
([`services/phd2-guider/src/client.rs`](../../services/phd2-guider/src/client.rs))
parses the result as a bare string, but PHD2 returns the object above. It
fails against real PHD2 every time and passes CI only because
`mock_phd2.rs` returns the same wrong shape. Fix the client, the mock
(which must also serve a real small FITS for the facade's tests) and the
`save_image` row in `phd2-guider.md` as the first commit of C6.

### D10. Shape of the facade

- **Where.** Inside the `phd2-guider` binary, as a second server: add
  `ascom-alpaca = { features = ["server", "camera"] }` alongside the
  existing axum service. The Alpaca server owns its own routing, discovery
  and management API, so it gets **its own port — proposed 11128** (free;
  sits in the driver block 11119–11127 rather than next to the rp-managed
  services, because what it serves *is* a driver-shaped device). It reuses
  the shared `AlpacaServerConfig` block like every other driver.
- **Opt-in.** `camera.enabled` defaults to `false`. An unrequested second
  Camera device in the roster is confusing, and enabling it costs PHD2
  round trips at startup.
- **Exposure.** `StartExposure(duration, light)` → `set_exposure(ms)` then
  `capture_single_frame`. `ImageReady` stays `false` until the app state
  returns to `Stopped`, then `save_image` → read → **delete** the file →
  decode to the `ImageArray` cache.
- **Capability surface.** `CanAbortExposure`/`CanStopExposure` `false`
  (PHD2 offers no cancel for a single frame), no cooler control, no
  gain/offset (PHD2 owns those through its equipment profile), bin 1 only.
  `CameraXSize`/`CameraYSize` from `get_camera_frame_size`. `MaxADU`
  65535. Passing ConformU with a surface this narrow is a real work item,
  not a footnote — budget for it in C6 the way `svbony-camera` did.
- **`PixelSizeX`/`PixelSizeY` come from config.** PHD2 exposes
  `get_pixel_scale` (arcsec/px), which is pixel size *divided by* focal
  length — not recoverable without the focal length, and only valid after
  calibration. rp reads `PixelSizeX` off the terminal camera for train
  optics, so the facade must be told: `camera.pixel_size_um`, required when
  `camera.enabled` is true, rejected at load if non-positive or non-finite.
  Document it as operator-entered from the guide camera's datasheet.
- **Same-host assumption (v1).** `save_image` writes on PHD2's host; the
  facade reads it back from the filesystem. `phd2-guider serve` colocated
  with PHD2 is the deployment we already document. A non-colocated setup
  fails with a structured `phd2_image_unreadable` error naming the path —
  never a silent empty frame. A shared/`image_dir` mapping is deferred.

### D11. The exclusivity contract (the part that must not be got wrong)

Guiding and single-frame capture are mutually exclusive *in PHD2*, so the
facade must make that explicit rather than resolve it:

- `StartExposure` while PHD2 is guiding or looping → an Alpaca
  `InvalidOperationException` naming the app state. **It must never stop
  the guide loop to service a capture.** Stopping guiding is an explicit
  operator/rp action (`POST /api/v1/guiding/stop`), never a side effect.
- `Connected = true` on the facade must not expose, not stop guiding, not
  touch the PHD2 profile — workspace tenet 3, *no actuation on connect*.
  Connecting only establishes the JSON-RPC session the guider service
  already holds.
- Conversely, `POST /api/v1/guiding/start` while a facade exposure is in
  flight waits for the frame or fails; it does not race `capture_single_frame`.
- Both paths share one PHD2 connection, so serialise them in `GuiderOps`
  behind the same lock, and report the loser with a structured error rather
  than queueing silently.

### D12. What this changes in rp (C7)

The guide camera becomes an ordinary `cameras[]` entry — `alpaca_url`
pointing at the facade, `device_number: 0` — and therefore a legal terminal
camera of the guiding train. Then:

- `auto_focus` addressed at the guiding train can run the **ordinary
  capture sweep** (`move_focuser` + `capture` + `measure_basic`), with the
  precondition *guiding stopped*.
- The existing **PHD2-metric sweep is kept, not replaced.** The two have
  opposite preconditions — the metric sweep needs an active guide loop, the
  capture sweep needs no guide loop — so they cover different moments:
  metric for mid-session refocus while guiding, capture for start-of-night
  focusing. `auto_focus` picks by current guiding state, or by an explicit
  parameter; that choice is C7's design-doc decision.
- `rp.md`'s flat statements that the guide camera "is never captured
  through — PHD2 may own it at the SDK level" (three places) become
  conditional on whether the facade is configured. The same sentence in
  [`optical-trains.md`](optical-trains.md) needs the same treatment.
- The Guide Focus Watch keeps reading `GuideStep` HFD; nothing there
  changes.

### D13. Why not the alternatives

- **`get_star_image` as the image source** — needs a selected star and
  gives a ≤32 px cutout. Fine for a star-profile display, useless for a
  focus sweep that must measure several stars across the field, and it
  cannot work before a star is selected (i.e. exactly when you want to
  focus).
- **A second Alpaca driver owning the guide camera, PHD2 reading through
  it** — PHD2 has no Alpaca backend on Linux/macOS at all, and on Windows
  only through a manually registered COM dynamic client. Not a path.
- **Time-slicing the guide camera between `qhy-camera` and PHD2** — the
  vendor SDK hands out a device once; "release it between exposures" means
  open/init churn against a camera in a control loop. This is the failure
  mode Part A exists to prevent, not a design.

---

## Open questions

1. **Facade port.** 11128 (driver block) as proposed, or 11132 (next to
   the rp-managed services 11130/11131)? The facade is served by an
   rp-managed service but *is* a device.
2. **Pixel size.** Config-required as in D10, or read once from a FITS
   header if PHD2's guide camera driver writes `XPIXSZ`? The header route
   removes an operator step but is vendor-dependent; D10 takes the
   explicit route.
3. **`claims` default.** `all` forever (back-compatible, D3), or warn
   after a release or two so multi-camera rigs are pushed toward explicit
   ownership?
4. **`device_number` reassignment and the `UniqueID` fallback change
   (D4.5, D4.6).** Both are breaking for existing multi-camera configs.
   Land them together inside C5 with one release note, or defer to a major
   version?

Settled by operator decision, recorded so the reasoning is not relitigated:

- **The USB port path is the only claim key** (D1). Serial and model are
  join signals and doctor display columns, never config surface.
- **The Windows port spelling is proven on hardware before any schema
  commits to it** — C1 is a blocking spike.

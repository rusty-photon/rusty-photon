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

**But Part A does not make Part B safe *on its own*, and the plan must not
imply it does.** `mode: "all"` stays the permanent default, and turning on
`camera.enabled` neither knows nor validates which port the guide camera
occupies — so a rig can run the facade *and* let the vendor driver claim
the same physical camera, which is precisely the conflict this plan
exists to end. Two things close that, both in C6/C7 scope:

- a **cross-service doctor check** (`claims.guide-camera-contested`):
  when a `phd2-guider` facade is enabled and a camera driver on the same
  host is on `mode: "all"` or claims a port PHD2 is using, say so and
  name the exclusion to add;
- the facade's own design doc stating the **explicit vendor-driver
  exclusion as a prerequisite**, not a recommendation.

Doctor is the right home because it already performs cross-service name
joins and is the only component that sees every service's config at once.

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| C0 | This plan | Not started | |
| C1 | **Hardware spike + passive USB identity**: confirm the Windows port spelling on the real box (direct and behind a hub, across replug and reboot), then implement `port` + `serial` extraction on all three collectors (new work on each — none extracts either today) and make inventory failure distinguishable from an empty bus | Not started | |
| C2 | `claims` schema + `svbony-camera` — the easy case, proves schema, join and doctor output | Not started | |
| C3 | `claims` in `zwo-camera` | Not started | |
| C4 | `claims` in `qhy-camera` + `qhyccd-rs` enumerate/probe split — restores the documented enumeration-only contract | Not started | |
| C5 | `doctor --devices` setup help + `claims.resolve` / `claims.unclaimed` / `claims.implicit` checks; the two breaking identity fixes together (port-based `UniqueID` fallback for serial-less cameras, claim-ordered `device_number`); `config.schema`/`config.apply` exposure | Not started | |
| C6 | `phd2-guider` Alpaca Camera facade on port 11128 (design doc → BDD → code): the completion watermark demonstrated against a live PHD2, the nested `camera` config block, `image_dir` + unit `ReadWritePaths=`, try-lock arbitration, and the catalog/packaging/firewall registration — plus the `save_image` wire-format fix | Not started | |
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
`phd2-guider.md` / `rp.md`, plus `doctor.md` for C5's new checks and
`packaging.md` for C6's new port), BDD second, implementation third.

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
replug and reboot before any config schema commits to a spelling.

Be precise about what is and is not already proven: the **collectors
exist and run passively on all three platforms**, but none of them
extracts a port or a serial today — `UsbDevice` carries only
`vendor`/`product`/`model`, the Linux walk discards the sysfs directory
name, macOS discards `location_id`, and Windows keeps only VID/PID and
the bus-reported description. So C1 implements and tests extraction on
**all three platforms**; what is unique about Windows is that the
*spelling itself* is unchosen, which is why it is the part that blocks.

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

Three mutually exclusive forms — each a complete value for the one
`claims` key, never combined:

```json
"claims": { "mode": "all" }
```

```json
"claims": { "mode": "include", "usb_ports": ["1-4.2"] }
```

```json
"claims": { "mode": "exclude", "usb_ports": ["1-4.3"] }
```

That is the whole surface. `mode: "all"` is the default **permanently** —
no deprecation, no future release that demands explicit claims — so every
existing config keeps **parsing** unchanged and a single-camera rig never
has to learn this block exists. Schema compatibility is not behavioural
compatibility, though: C5 deliberately changes the identity of
serial-less cameras and the device-number assignment of multi-camera
rigs, so an existing file can stay valid while the bindings it feeds —
rp's `cameras[].device_number`, `devices` overrides keyed on a
`noserial-*` identity — still need revisiting. See the breaking-change
note under D4. The nudge toward explicitness is doctor's
`claims.implicit` finding (D5), which fires only when a driver on `all`
enumerated more than one device. `usb_ports` is required for `include` and
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
   never writes them. Both comparisons need per-SDK canonicalization
   before they mean anything: QHY's id is `QHY268M-<serial>`, not a bare
   serial, so it must be split before being compared with the USB
   descriptor's serial, and model strings differ between the SDK's name
   and the bus-reported product string. Each driver supplies its own
   normalizer, with unit tests over real observed pairs — an
   un-normalized compare reports a claimed port unresolvable while
   staring at the right camera.
3. **An unresolvable join is reported, never guessed.** Registering the
   wrong camera is worse than registering none: a guess means the guide
   camera answers to the main camera's Alpaca device number, and the first
   symptom is a subframe from the wrong sensor at 2am.

   The topologies that cannot be resolved are **per SDK**, because what
   is readable passively differs:

   - **Any SDK — two identical models, both reporting no serial, both
     connected at once.** No signal is left to tell SDK index 0 from
     index 1, and no amount of doctor output invents one.
   - **ZWO — two identical models, *even when both have serials*.**
     `ASIGetSerialNumber`/`ASIGetID` are only readable after
     `open_uninitialised()`, so disambiguating by serial would mean
     opening each candidate — including the camera the claim exists to
     leave alone. That is the one rule (D4.1) which cannot bend, so the
     serial is simply unavailable for this decision and the model
     collision stands.

   Both are **explicitly unsupported** for claims: the driver reports the
   collision and registers neither camera. This is not a gap D5 closes —
   D5's unplug procedure identifies which *port* a camera sits in, which
   is a different question (see D5). A rig in either topology either
   separates the models or lets `mode: "all"` claim both.
4. **An unavailable USB inventory is not an empty bus.** Every collector
   returns an empty `Vec` when its source fails — sysfs unreadable,
   `system_profiler` missing, PowerShell erroring — and an empty result
   read as "no devices" would either register nothing or, worse, invite a
   fallback that opens every SDK camera and defeats the entire ownership
   boundary. The inventory must therefore distinguish *failed* from
   *empty*, and a failed inventory **fails closed**: no device is opened,
   the service reports the inventory error, and doctor names it. This
   requires changing the collectors' signatures to a `Result`, which is
   part of C1.
5. **A claimed port with nothing in it** registers nothing, logs `warn!`,
   and produces a *soft* doctor finding. Never a startup failure: a
   powered-down hub is a normal Tuesday, and a driver that refuses to
   start because one of three cameras is absent is worse than one that
   starts with two.
6. **The port strengthens `UniqueID` for serial-less cameras.**
   `zwo-camera`'s `mint_identity` falls back to `noserial-{index}` when a
   camera exposes neither serial nor flash id, and its own doc comment
   names the weakness: *"two serial-less cameras of the same model
   reordered on the bus … could swap identities."* With the port in hand
   that fallback becomes `noserial-{port}`, which is stable across bus
   reordering and replug. Same change for the other two drivers'
   equivalents. Small, obviously right, and it closes a documented
   ambiguity — but it **changes `UniqueID` for affected cameras**, so it
   lands with C5 and its documentation, not silently inside a driver
   phase (see the note on breaking changes below).
7. **`device_number` stability — for explicit claims only.** Alpaca
   device numbers are assigned by enumeration order today, so unplugging
   one camera renumbers the others and silently re-points every
   `cameras[].device_number` in rp's config. **Under `mode: "include"`
   only**, assign by the claim's **position in `usb_ports`**, which is
   the operator's declaration and does not move when a camera is absent.
   That requires stating the consequence explicitly, because it is the
   whole mechanism: **device numbers are sparse and are never
   compacted.** With a claim of `["1-4.2", "1-4.3"]` and the first camera
   unplugged, the survivor stays device number 1 and device number 0
   simply does not exist that night. Alpaca device numbers need not be
   contiguous — they are path segments, not array indices — so a gap is
   legal, and compacting is what silently re-points rp's config.

   **`exclude` and `all` get no stability guarantee**, and the reason is
   structural rather than an omission: `exclude`'s `usb_ports` names the
   devices to *omit*, so it declares no ordering for the survivors at
   all, and any fallback to enumeration or sort order renumbers a later
   camera when an earlier one is unplugged. A multi-camera rig that wants
   stable device numbers uses `include` — which is also the mode that
   states ownership positively, so the two properties arrive together. Under `mode: "all"` sorting by port is only a
   partial fix and the plan should not claim otherwise: with cameras at
   A/B/C, removing B still moves C from 2 to 1. **`mode: "all"` therefore
   offers no device-number stability guarantee**, and a multi-camera rig
   that wants one uses an explicit claim list. Breaking for existing
   multi-camera configs (see below).

Note the deliberate asymmetry: **claims are port-keyed, while the
`devices` override map and the ASCOM `UniqueID` stay serial-derived —
for every camera that has a real serial.** `UniqueID` is a shipped
contract that rp and every ASCOM client stores; re-keying it on the port
would mean a device's identity changed when its cable moved, which is
exactly wrong for identity even though it is exactly right for a claim.
The two vocabularies answer different questions — *which socket is this?*
versus *which camera is this?*

**For a camera with no serial the two do meet, and the consequence must
be stated plainly.** D4.6 replaces the `noserial-{index}` fallback with
`noserial-{port}`, and in all three drivers that minted string is *both*
the `UniqueID` suffix **and** the key of the `devices` override map
(`zwo-camera/src/lib.rs` `mint_identity`, `svbony-camera/src/lib.rs`
likewise). So for serial-less cameras the change moves both: an existing
`devices` entry keyed `noserial-0` must be re-keyed to `noserial-1-4.2`,
and the `UniqueID` such a camera publishes changes with it. C5 carries
the migration note; the alternative — a port-based `UniqueID` beside an
index-based override key — would leave the two permanently inconsistent.

**Both breaking changes (D4.5, D4.6) land together in C5, now.** The
workspace is at 0.1.0 with no published CHANGELOG and a handful of known
rigs: this is the cheapest these changes will ever be, and both fix
ambiguities the code already documents as flaws — a scan-order device
number that silently re-points rp's config, and an identity that two
serial-less cameras can swap. Deferring would ship the known-wrong
behaviour into 1.0 and then break more people. One disruption, one upgrade
step: re-run `doctor --devices` and fix device numbers once. The upgrade
note lives in the C5 PR body and in each driver's design doc, since there
is no CHANGELOG to carry it.

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

**The Serial column is the USB descriptor's serial, and it is often
blank.** `--devices` is enumeration-only, so it cannot open a ZWO camera
to mint an SDK serial — and must not, least of all for a camera it does
not claim. What it prints is whatever the passive USB scan carries
(`serial` from sysfs / PnP), which some cameras publish and some do not.
The column is therefore advisory: `—` means "not published on the bus",
never "this camera has no identity". The SDK id column is likewise
populated only for devices the driver claims and has enumerated through
the SDK.

Two checks join the per-service set, alongside `config.full-shape` and
`hardware.sdk-devices`:

| Check | Trigger |
|---|---|
| `claims.resolve` | A claimed port holds no device (`warn` — absent hardware or a moved cable), or holds one that cannot be joined to an SDK device (`fail` — the driver will register nothing for it). |
| `claims.unclaimed` | Devices on the bus that no claim covers, listed for information (`ok`) — so "why is my camera missing" answers itself. |
| `claims.implicit` | The driver is on `mode: "all"` **and** enumerated more than one device: names the devices it just claimed and prints the paste-ready block for claiming a subset (`ok`, informational). Silent on a single-device rig, where nothing is ambiguous; informative exactly where ownership could be contested. Reuses the listing above rather than building anything new. |

An automated *drift* check ("this port used to hold a QHY268M") is
deliberately **not** in scope: with the port as the only key there is
nothing in the config to compare against, and inventing a remembered-state
file to enable one check is not worth it. The listing above shows the
operator what is where; a moved cable shows up as a `claims.resolve` warn
plus an unfamiliar model in the table.

**The unplug procedure answers "which port is this camera in?", not
"which SDK slot is this camera?"** When the operator cannot tell which
row is which physical camera, doctor tells them to unplug one and re-run:
the port that disappears is the one they just unplugged. That is enough
to *write the claim*, and it is all this procedure does.

It does **not** rescue the unsupported topology in D4.3. Two identical
serial-less cameras plugged in together still offer no join signal once
both are back on the bus, so the driver still refuses both. Doctor says
exactly that, rather than implying another re-run would help: *"two
QHY5III715C report no serial; claims cannot address either while both are
connected."*

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
registering. Serial is free at enumeration, so the join resolves
*whenever the camera reports one* — but `mint_identity` already falls
back to `noserial-{index}` for an empty `CameraSN`, so SVBony is not
immune to the D4.3 topology either. It is still the easiest driver to
prove the schema, the join and the doctor output against, because
nothing has to be opened to reach the serial.

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

So the facade is `set_exposure` + `capture_single_frame` + wait +
`save_image` + decode FITS. There is no full-frame-over-the-wire path and
no "frame done" event.

**Waiting for `AppState == Stopped` does not work, and the reason is
structural.** `capture_single_frame` is only legal when guiding and
looping are *already* stopped — so the state is `Stopped` before the
exposure, during the wait, and after it. A poll that merely observes
`Stopped` is satisfied instantly and `save_image` then returns **the
previous frame**, silently, with every field of the FITS looking
plausible. On a focus sweep that means every position reports the
previous position's HFD: a sweep that converges confidently on the wrong
number.

C6 must therefore establish a real completion watermark before saving.
Candidates, in order of preference, to be settled during C6's design
phase against a live PHD2:

1. A **frame-counter watermark** — `LoopingExposures` carries a `Frame`
   number; if `capture_single_frame` emits one, capture the pre-exposure
   value and wait for it to advance.
2. An observed **transition** — wait for the state to leave `Stopped`
   and return, which requires the poll to be fast enough not to miss a
   short exposure, so it needs a measured poll interval and is the
   weaker option.
3. Failing both, **`save_image` + the FITS `DATE-OBS`/exposure header**
   compared against the request — treat a frame older than the request as
   not-yet-ready and retry.

This is the single largest unknown in Part B, and C6's design-doc phase
does not end until one of these is demonstrated against a real PHD2.

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
  and management API, so it gets **its own port: 11128** — joining the
  Alpaca device block (11119–11127) rather than sitting next to the
  rp-managed services on 11130/11131. A port number should say what a
  client will find there, and what is there is an ASCOM Camera: a client
  sweeping the driver range finds every camera in the rig, this one
  included. That `phd2-guider` happens to host it is an implementation
  detail no client sees.

  **It does not "reuse the existing server block" — that was wrong.**
  `phd2-guider`'s `Config::server` is a `rusty_photon_server_config::ServerConfig`
  for the axum REST service, defaulted to 11130; it is not an
  `AlpacaServerConfig` and there is only one of it. C6 adds a **second,
  nested block** so the two listeners are configured independently:

  ```json
  "camera": {
    "enabled": false,
    "pixel_size_x_um": 3.75,
    "pixel_size_y_um": 3.75,
    "image_dir": "/var/lib/rusty-photon/phd2-images",
    "server": { "port": 11128 }
  }
  ```

  The nested `server` is the Alpaca block (port, TLS, auth), parallel to
  the existing one rather than replacing it. Both listeners bind under
  the same `ServiceRunner` and stop on the same shutdown signal; the
  facade's failure to bind is fatal only when `camera.enabled` is true.

  **UDP discovery stays off**, matching every other Alpaca server in the
  fleet: `docs/packaging.md` disables it deliberately because this many
  same-host Alpaca servers collide on the shared discovery port, and
  clients are pointed at `host:port` directly from the port table. The
  facade is one more row in that table, not an exception to the rule.

- **11128 is a packaging and catalog change, not just a config field.**
  Today `services/phd2-guider/pkg/doctor.toml` declares `class = "core"`
  with a single `port = 11130`, `docs/workspace.md` lists the service with
  no Alpaca port at all, and `installer/fragments/phd2-guider.wxs` opens
  only TCP 11130. With the facade enabled, doctor's port-collision check
  would not know about 11128, the Windows firewall would not admit it, and
  and clients pointed at the port table would not find 11128 listed.
  C6 therefore includes: the catalog entry, the workspace index row, the
  port table row in `packaging.md`, the `.wxs` firewall exception, and
  the Linux packaging notes — or, if that is judged too much for one
  phase, an explicit decision to bind the facade loopback-only and say so
  in the design doc.
- **Opt-in.** `camera.enabled` defaults to `false`. An unrequested second
  Camera device in the roster is confusing, and enabling it costs PHD2
  round trips at startup.
- **Exposure.** `StartExposure(duration, light)` → `set_exposure(ms)` then
  `capture_single_frame`. `ImageReady` stays `false` until the completion
  watermark of D9 is observed, then `save_image` → validate the path →
  read → decode to the `ImageArray` cache → **delete** the file.
- **The wait is bounded.** `requested exposure + camera.capture_grace`
  (default a few seconds), after which the exposure fails with a
  structured error, `ImageReady` stays false, the in-flight state is
  cleared and the arbitration lock is released. Without a deadline a
  wedged PHD2 or a missed watermark parks the exposure forever — and
  because the facade shares arbitration with guiding (D11), that would
  also block `guiding/start` for the rest of the night.
- **File cleanup is a guard, not a happy-path step.** Deletion runs on
  every exit from the capture — success, read error, decode error,
  timeout, cancellation — not only after a successful decode. An
  overnight sweep that fails at the decode step must not leave a FITS per
  attempt on PHD2's host. A failed deletion is logged, never fatal.
- **The returned path is validated before it is touched.** `save_image`'s
  filename arrives from the PHD2 RPC peer, and the facade both reads and
  unlinks it. It must be canonicalized and required to sit inside
  `camera.image_dir`, be a regular file, and not be a symlink; anything
  else is refused with a structured error and nothing is read or deleted.
  PHD2 is a local trusted process in the normal case, but "reads and
  deletes an arbitrary path a peer names" is not a property to leave
  unbounded in a service running as its own user.
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
  optics, so the facade must be told. **Two fields, not one:**
  `camera.pixel_size_x_um` and `camera.pixel_size_y_um` — `PixelSizeX`
  and `PixelSizeY` are separate ASCOM properties and rp caches them as
  separate invariants (`pixel_size_x_um`, `pixel_size_y_um` in
  `services/rp/src/equipment/camera.rs`). A single value would quietly
  advertise square pixels for a rectangular sensor and corrupt one axis
  of the train optics. Both required when `camera.enabled` is true, both
  rejected at load if non-positive or non-finite.
  Document it as operator-entered from the guide camera's datasheet. The
  config value is the **only** source: cross-checking it against a FITS
  `XPIXSZ` header was considered and dropped — it would add a second
  source of truth, and a warning nobody reads, for a value the operator
  types once per rig.
- **Same-host is necessary but *not sufficient* — the documented
  deployment already breaks it.** `docs/packaging.md` § "phd2-guider:
  PHD2" tells the operator to run PHD2 headless under TigerVNC from their
  own desktop session (`~/.vnc/xstartup`), while the packaged guider unit
  runs `User=rusty-photon` with `ProtectHome=yes`. PHD2 therefore writes
  its FITS under a home directory the service is *structurally forbidden*
  to read — colocation does not help, and a v1 that assumed it would have
  failed on the reference rig at the first capture.

  So `camera.image_dir` is **required, not deferred**: an explicit
  directory both parties can reach (e.g.
  `/var/lib/rusty-photon/phd2-images`, group-owned by `rusty-photon`),
  with PHD2 configured to save there and the packaged unit granted access
  via `ReadWritePaths=`. C6 owns the unit change and the operator
  instructions in `packaging.md` alongside the code.
- **Misconfiguration fails at load, not at 2am.** With
  `camera.enabled: true`, a `phd2.host` that is not local, or an
  `image_dir` that is absent or unwritable, is a deterministically broken
  configuration — the workspace's fail-fast posture says reject it at
  config load with a message naming the fix, rather than starting happily
  and failing on the first exposure with `phd2_image_unreadable`. That
  error remains, for the runtime cases load-time validation cannot see
  (the directory disappearing, a permission change mid-session).

### D11. The exclusivity contract (the part that must not be got wrong)

Guiding and single-frame capture are mutually exclusive *in PHD2*, so the
facade must make that explicit rather than resolve it:

- **The allowed pre-capture state is an allowlist, not a denylist.**
  `capture_single_frame` is legal only when guiding *and looping* are
  stopped, and PHD2's `AppState` has more states than `Guiding` and
  `Looping`: `Paused` is the trap, because `set_paused(full: false)`
  pauses corrections while **looping continues**, so a paused-but-looping
  PHD2 would pass a "not guiding, not looping" check and still reject the
  RPC. The facade therefore permits capture from `Stopped` alone —
  everything else (`Guiding`, `Looping`, `Paused`, `Calibrating`,
  `Selected`, `LostLock`) is refused with an Alpaca
  `InvalidOperationException` naming the state it saw. An allowlist fails
  safe when PHD2 adds a state; a denylist fails open.
- **It must never stop the guide loop to service a capture.** Stopping
  guiding is an explicit operator/rp action
  (`POST /api/v1/guiding/stop`), never a side effect.
- `Connected = true` on the facade must not expose, not stop guiding, not
  touch the PHD2 profile — workspace tenet 3, *no actuation on connect*.
  Connecting only establishes the JSON-RPC session the guider service
  already holds.
- Conversely, `POST /api/v1/guiding/start` while a facade exposure is in
  flight must not race `capture_single_frame`.
- **Sharing `GuiderOps`'s existing `op_lock` is not enough, and saying so
  was wrong.** That mutex is taken with `lock().await` and its contract is
  explicit: *"the mutating operations serialize behind a single-flight
  mutex (overlapping requests queue, not error)"*
  (`services/phd2-guider/src/service/guider.rs`). Queuing is right for two
  guiding operations — a dither behind a stop is fine — but wrong here: a
  `guiding/start` parked behind a 10-second guide-camera exposure is
  indistinguishable from a hung service, and the caller has no way to know
  why. C6 specifies **`try_lock` arbitration for the capture path**: the
  exposure takes the lock if free and otherwise fails immediately with a
  structured `busy` error naming the operation in flight; `guiding/start`
  likewise fails fast rather than queueing behind an exposure. The
  existing guiding-to-guiding queueing contract is unchanged — this is a
  new rule for the capture↔guiding pair only, and both directions get a
  BDD scenario.
- **Stop is privileged.** `guiding/stop` and the safety path must never be
  refused because an exposure holds the lock: they proceed, and the
  exposure they interrupt fails with its structured error. A safety stop
  that can be blocked by a focus frame is not a safety stop.

### D12. What this changes in rp (C7)

The guide camera becomes an ordinary `cameras[]` entry — `alpaca_url`
pointing at the facade, `device_number: 0` — and therefore a legal terminal
camera of the guiding train. Then:

- `auto_focus` addressed at the guiding train can run the **ordinary
  capture sweep** (`move_focuser` + `capture` + `measure_basic`), with the
  precondition *PHD2 in `Stopped`* — not merely "not guiding". Per D11 a
  looping or partially-paused PHD2 refuses the capture, so the sweep would
  fail at the first frame. rp enforces the precondition before the sweep
  starts (a single `get_app_state` read) and reports it as a refusal to
  start, rather than discovering it frame by frame. rp does **not** stop
  guiding to satisfy it: that is the operator's or the workflow's call.
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
- **The mount motion gate must learn about guide captures.** rp's
  `imaging_permit` returns `None` for a camera in the guiding train —
  guide-train exposures deliberately bypass the gate, and the code
  comment says why: *"Un-trained and guiding-train cameras bypass the
  gate — trains are enrichment, not a gate"*, written when the guide
  train never captured. The moment a capture sweep runs through the guide
  camera, that exemption becomes a defect: a dither or slew can move the
  mount mid-exposure and corrupt the focus sample, and the sweep would
  fit a curve through trailed stars. C7 must either admit guide-train
  captures to the gate as shared holders (the imaging-train treatment) or
  hold off all mount motion for the duration of the sweep. This is a
  change to `imaging_permit`'s contract and to `rp.md` § Mount Motion
  Gate, not an incidental fix.
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

## Decisions

Settled with the operator; recorded so the reasoning is not relitigated in
review.

| # | Decision | Why |
|---|---|---|
| 1 | **The USB port path is the only claim key** (D1) | Serial does not exist for every camera (the ASI1600 exposes neither serial nor flash id); three ways to name one device means three code paths and a config whose meaning depends on which key the author reached for. Serial and model stay as internal join signals and doctor display columns, never config surface. |
| 2 | **C1 is a blocking hardware spike** (D2) | The Windows port spelling is the one leg not already proven by code in this repo. Prove it on the real box — direct and behind a hub, across replug and reboot — before any schema commits to a spelling. An unstable key on one platform is worse than no key. |
| 3 | **The facade listens on 11128** (D10) | It joins the Alpaca device block because a port should say what a client finds there, and what is there is an ASCOM Camera. Its hosting process is not a client-visible fact. The port is a second listener under a new nested `camera.server` block — not a reuse of the existing REST `server` — and it brings catalog, packaging and firewall registration with it. |
| 4 | **`PixelSizeX`/`Y` come from config alone — as two fields** (D10) | ASCOM clients and ConformU read `PixelSizeX` right after connect, before any exposure, and tenet 3 forbids capturing a frame on connect to discover it. A FITS-header cross-check was dropped as a second source of truth for a value typed once per rig. `pixel_size_x_um` and `pixel_size_y_um` are separate because ASCOM and rp treat them as separate invariants; one value would advertise square pixels for a rectangular sensor. |
| 5 | **`mode: "all"` is the permanent default** (D3, D5) | No deprecation and no future release demanding explicit claims: existing configs never break and single-camera rigs never meet the block. Doctor's `claims.implicit` finding nudges only the multi-device case, where ownership can actually be contested. |
| 6 | **Both breaking changes land in C5, at 0.1.0** (D4) | Pre-1.0, no CHANGELOG, few rigs — the cheapest this will ever be, and both fix ambiguities the code documents as flaws. One disruption, one upgrade step. |

Nothing in this plan is waiting on an answer. C1 is waiting on hardware,
and C6's design phase is waiting on one measurement against a live PHD2
(the completion watermark, D9).

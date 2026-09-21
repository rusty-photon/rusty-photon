# Device Claims + the PHD2 Guide-Camera Facade

## Goal

Two problems that look unrelated share one root: **the guide camera is the
only device in the rig that no Alpaca driver may own.**

1. **Every SDK-backed camera driver claims every camera its SDK
   enumerates.** (Cameras only — the focuser and filter-wheel drivers
   are out of scope, see D8.)
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

- the facade's own design doc stating the **explicit vendor-driver
  exclusion as a hard prerequisite**, not a recommendation: enabling
  `camera.enabled` without excluding the guide camera from its vendor
  driver is a misconfiguration, and the facade's design doc says so;
- **the conflict staying legible at runtime**: if both the vendor driver
  and PHD2 want the same camera, whichever opens **second** fails and
  reports a named error naming the camera. Be precise about the limit —
  the winner never learns anything, because nothing tells a process that
  someone else wanted the device it already holds. So the diagnosis is
  one-sided and arrives wherever the loser logs: useful, and not a
  substitute for getting the claim right. With the doctor check
  withdrawn there is no cross-service status path, which is part of the
  cost of respecting ADR-016 and is recorded as such.

**A cross-service `doctor` check was proposed here and is withdrawn: it
violates an accepted architecture decision.** `docs/services/doctor.md`
says doctor audits service facts and *"never learns device usage (which
camera is the guide cam belongs to `rp`)"*, and
[ADR-016](../decisions/016-service-config-ownership-and-doctor.md)
decision 4 is explicit: *"Which camera is the guide cam, dark-library
setpoints, focal length, and device identity binding are **usage**, owned
by `rp`. Doctor never needs to know a serial exists."* A
`claims.guide-camera-contested` check — and the
`camera.guide_camera_usb_port` field that would feed it — is exactly
"which camera is the guide cam" moved into doctor's config surface.

There is a real argument for it (doctor already reports TCP port
collisions, and "two services claim one USB port" has the same shape),
but it is an argument for **amending ADR-016**, not for quietly
contradicting it. That amendment is out of this plan's scope and is the
repo owner's call; this plan therefore keeps doctor out of device usage
and relies on the prerequisite plus the runtime error. If the amendment
is ever made, the check and its config field are the obvious follow-up —
recorded here so the option is not lost.

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| C0 | This plan | Not started | |
| C1 | **Hardware spike + passive USB identity**: confirm the Windows port spelling on the real box (direct and behind a hub, across replug and reboot), then implement `port` + `serial` extraction on all three collectors (new work on each — none extracts either today) and make inventory failure distinguishable from an empty bus | Spike partly done — see D2 | `chore/device-claims-c1-spike` |
| C2 | `claims` schema + `svbony-camera` — the easy case, proves schema, join and doctor output | Not started | |
| C3 | `claims` in `zwo-camera` | Not started | |
| C4 | `claims` in `qhy-camera` + `qhyccd-rs` enumerate/probe split — restores the documented enumeration-only contract, **and moves the CFW probe off startup entirely** (the split alone narrows the tenet-3 problem, it does not discharge it) | Not started | |
| C5 | `doctor --devices` setup help + `claims.resolve` / `claims.unclaimed` / `claims.implicit` checks; the port-based `UniqueID` fallback for serial-less cameras (ZWO/SVBony; QHY's case decided here per D4.6); **a device-number strategy selected from D4.7's three options** — the sparse-slot approach is not implementable against the pinned server; `config.schema`/`config.apply` exposure | Not started | |
| C6 | `phd2-guider` Alpaca Camera facade on port 11128 (design doc → BDD → code): the completion watermark demonstrated against a live PHD2, the nested `camera` config block, `image_dir` + unit `ReadWritePaths=`, try-lock arbitration, and the catalog/packaging/firewall registration — plus the `save_image` wire-format fix | Not started | |
| C7 | rp wiring: guide camera as a train-terminal camera, capture-sweep focusing for the guiding train, doc updates. **Blocked on reconciling with [`focus-model.md`](focus-model.md) S7/D17**, which retires rp's capture-based `auto_focus` and keeps the metric sweep under that name | Not started | |
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
`doctor.md` is updated **three times** — in C6 as well, because the
optional second listener changes the catalog schema and the
port-collision semantics, and leaving that undocumented would break the
design-doc-first rule for the most operator-visible part of C6. In C1,
because the shared facts contract changes there (`UsbDevice` gains `port`/`serial` and inventory
errors stop being folded into an empty bus — central-doctor behaviour
that must not land undocumented), and again in C5 for the new
`claims.*` checks. `packaging.md` is updated in C6 for the new port and
the `image_dir` sharing steps.

---

## Part A — Device claims

### D1. The USB port path is the only key

Surveyed against the three vendored SDK crates and the host USB layer
rather than assumed:

| Candidate key | Exists for every camera? | Readable without opening the device? | Ambiguous? |
|---|---|---|---|
| **USB port path** | **Yes** — every device on the bus has one | **Yes**, on every platform | No — one device per port |
| SDK serial | No — many cameras report none | ZWO: no (needs `ASIOpenCamera`). QHY/SVBony: yes | No, when it exists |
| Model name | Yes | Yes | Yes — and worse than that on Windows, where two *different* QHY models share one bus-reported name (D2) |

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
(D4) and **doctor display columns** (D5). What they are never is a
**`claims` key**. The `devices` override map keeps its existing
serial-derived keys for cameras that have a real serial (D4.6 changes
only the serial-*less* fallback), so C5 must not read this decision as
licence to re-key or remove valid overrides.

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

So Part A extends `UsbDevice` with `port` (the key) and `serial` (a join
signal), and reuses the existing inventory. **No new crate dependency, no
`crate_universe` repin.**

**`port` is optional only in the serialized shape, never at runtime.** A
candidate device that reached the claims resolver without a port would be
indistinguishable from one whose port simply did not match a claim — a
silent mis-resolution in the one field ownership depends on. So a missing
`port` on a candidate record **is** an inventory failure (D4.4), caught
before any SDK open; `Option` survives only as a compatibility shim for
older serialized fixtures, which the runtime path rejects.

**C1 spike results — `rig2` (Starfront, Windows 11 Pro 26200, Intel
NUC12WSHi3), 2026-09-21.** Read entirely from the PnP cache with
`Get-PnpDevice` + `Get-PnpDeviceProperty`; nothing was opened. The rig
presented the contested case directly — a QHY600M (`1618:C601`) and a
QHY5III678M (`1618:0679`) behind one SDK, plus a ZWO ASI662MC
(`03C3:662B`), all three behind the UPBv2's hub:

```
QHY600M       PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)   Port_#0001.Hub_#0003
QHY5III678M   PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(3)   Port_#0003.Hub_#0003
ASI662MC      PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(2)#USB(4)    Port_#0004.Hub_#0004
```

1. **`DEVPKEY_Device_LocationPaths` is the property — settled.** It
   carries the full chain including every hub hop, where
   `DEVPKEY_Device_LocationInfo` carries one level only
   (`Port_#0003.Hub_#0003`) — it names the port and the hub's index but
   not the path to that hub, so two devices on identically-numbered ports
   of different hubs are indistinguishable by it. That depth is the
   disqualifier, and it is structural. (Its `Hub_#n` *looks* like an
   enumeration index that would drift, but it did not move across the
   reboot in (3), so that is not the reason to reject it.) D2's candidate
   comparison closes in `LocationPaths`' favour and the schema can commit
   to that spelling.

2. **It is multi-valued, and the `PCIROOT` element is not guaranteed.**
   Every healthy device returned two spellings — a `PCIROOT(…)` chain and
   an `ACPI(_SB_)#ACPI(PC00)#ACPI(XHCI)#…` chain — but a device whose
   descriptor request had failed returned **only** the ACPI form. So the
   Windows collector must *select* the `PCIROOT(`-rooted element rather
   than take index `[0]`, and must define what a device with no such
   element is: a candidate record with no usable port, which D4.4 already
   classifies as an inventory failure. This shape was not anticipated —
   the plan described the property as though it held one string.

3. **Stable across a port power cycle, and unaffected by a sibling's
   absence.** Switching the UPBv2 USB port feeding the QHY5III678M off
   and back returned it to a byte-identical `LocationPaths` and
   `LocationInfo`; the other two cameras kept their exact paths while it
   was gone, and again after it returned. The same held for the
   ASI662MC's port. That is the property Alpaca device numbers lack —
   [#1184](https://github.com/rusty-photon/rusty-photon/issues/1184) is
   the same rig renumbering its cameras after a power cycle — and it is
   the direct evidence for D4.7's motivation.

   **It also survives a host reboot.** The box was restarted with the
   UPBv2 still powering all three cameras, so Windows rebuilt its USB
   tree from scratch against an unchanged bus. Every one of the three
   came back on a byte-identical `LocationPaths`, `LocationInfo` *and*
   device instance id. What this leg does not cover is the cameras
   themselves re-enumerating from cold, since the UPBv2 held them up
   throughout; and a genuine move to a *different* port still needs
   physical access. With those two exceptions the key is proven stable
   on Windows across every transition C1 set out to test.

4. **Windows publishes no USB serial for any of the three cameras.** The
   third field of the instance id is a port-derived string
   (`6&4213695&0&3`), not a serial — contrast the UPBv2's own FTDI
   bridge, enumerated as `USB\VID_0403&PID_6015\UPB248E11M`, which
   carries a real one. So on Windows, D5's Serial column is `—` for every
   camera on this rig, and D4.2's serial join is simply unavailable:
   QHY's serial exists only behind `GetQHYCCDId`, which is SDK-side.
   D1's per-SDK table is about *SDK* serials and remains correct; what is
   new is that the **bus** supplies no serial to join against, on the one
   platform where the join has the least to work with.

5. **The model join signal is weaker on Windows than D1 assumed, and
   VID:PID is the repair.** Both QHY cameras report the *same*
   bus-reported name, `QHY5IIISeries_IO` — two **different** models
   colliding, not the "two of a kind" case D1 anticipated, so a
   model-string join would find the bus ambiguous while the SDK sees two
   plainly distinct cameras. Their **product ids differ** (`C601` vs
   `0679`), and `UsbDevice` already carries `vendor`/`product`. So the
   join described in D4.2 should read VID:PID before the model string,
   and each driver's normalizer maps its own SDK model names onto the
   product ids it expects. Unit tests over observed pairs get a real
   fixture from this rig.

6. **The UPBv2 presents two hubs, and which one a camera appears under is
   not yet explained.** The box enumerates as a Microchip companion pair
   on one upstream connector — `USB2807 Hub` at `USB(2)` (the NUC's
   `HS02`) and `USB5807 Hub` at `USB(14)` (`SS02`) — with the UPBv2's own
   FTDI bridge on the USB 2.0 half at downstream port 7. The vendor
   specifies four USB 3.1 and two USB 2.0 connectors. Observed: the two
   QHY cameras sit under the `USB5807` half at downstream ports 1 and 3,
   and the ASI662MC under the `USB2807` half at port 4 — each confirmed
   by switching its UPBv2 port off and watching that device, and only
   that device, leave the bus. Downstream port numbers match the UPBv2's
   own port labelling on both halves.

   **What that does not establish**, and what the schema should not
   assume either way: whether a camera's hub segment is fixed by which
   connector it occupies, or can differ between enumerations of the same
   camera in the same connector. Distinguishing the two needs an
   observation this spike does not have — a device seen under both halves
   — and it cannot be got from a static reading, since a USB 2.0 device
   in a USB 3.1 connector and any device in a USB 2.0 connector are
   indistinguishable once enumerated. Until something demonstrates
   otherwise, the port path is treated as stable, which is what the
   power-cycle evidence in (3) actually showed. Which of this rig's
   connectors are the USB 2.0 pair is likewise unknown — the vendor does
   not document the panel layout, so it needs a look at the hardware.

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
everywhere else in the config tree. Entries are validated at load:
**non-empty and unique**. A repeated port (`["1-4.2","1-4.2"]`) makes
registration and any positional numbering ambiguous, and an empty string
can never resolve to a real port — both are config errors, caught before
any SDK work rather than producing a confusing runtime state. An `include` with an empty list
registers nothing: legal, and logged loudly — it is how an operator hands
a whole SDK to another application for a night. **It is also exempt from
D4.4's fail-closed rule**, since it opens nothing and needs no join:
refusing to start because a USB scan is down, when the configuration says
"claim nothing", would be a failure with no possible consequence to
prevent. The same exemption applies to `exclude` with an empty list,
which is `all` spelled differently.

The reference rig's `qhy-camera.json` becomes one line — and it uses
`include`, naming the camera this driver owns, **not** `exclude` naming
PHD2's:

```json
"claims": { "mode": "include", "usb_ports": ["1-4.2"] }
```

`exclude` appears elsewhere in this section as a mechanism, but D5
explains why it is the wrong choice whenever another application owns one
of the cameras: exclusion is negative, so a moved camera lands on an
unlisted port and gets opened.

A comment naming the camera is the operator's business; the config does
not carry a second name for the same device that could fall out of date.

### D4. Behaviour

1. **Claims are applied before any device-touching call.** The one rule
   that cannot be relaxed per driver. The USB inventory yields ports
   passively; the filter runs; only survivors are opened, probed or
   initialised.

   **This makes the *reload* path the dangerous one, and claims must not
   be hot-appliable until it is fixed.** Exposing `claims` through
   `config.apply` (C5) invites a mid-session change, and `qhy-camera`'s
   reload rebuilds `ServerBuilder` and the `Sdk` — rerunning
   open→`InitQHYCCD`→CFW against every *claimed* camera, tearing down a
   live handle and triggering the documented QHY init side effects
   (CFW auto-home among them) on a camera that may be mid-exposure.
   Filtering unowned cameras does nothing about that. So C5 either makes
   `claims` **restart-only** (`config.apply` accepts the change, persists
   it, and reports that it takes effect on restart — the cheap, safe
   default) or adds a reload path that preserves active handles and
   re-probes nothing already open. Restart-only is the recommendation:
   an ownership change is a deliberate, rare act, and pairing it with a
   service restart costs an operator seconds.

   **"Restart-only for `claims`, reload for everything else" is not
   expressible today.** `ConfigurableDriver` exposes a single
   `apply_disposition()` per driver (`rusty-photon-config/src/actions.rs`,
   defaulting to `Reload` for the Alpaca drivers), and `config.apply`
   classifies the whole change set by it — there are no field-level
   dispositions. So C5 picks one and documents it: make **all** camera
   config restart-only (blunt, and it would regress today's reloadable
   settings), or extend the shared config-actions API with **per-path
   dispositions** (the general fix, which every driver with a mix of hot
   and cold settings will eventually want). The choice must be made
   before C5 exposes `claims` through `config.apply`, not discovered
   during it.
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

   Both are **unsupported by `include`/`exclude`**: those modes must map
   a port to an SDK device, the map is ambiguous, and the driver reports
   the collision and registers neither camera. This is not a gap D5
   closes — D5's unplug procedure identifies which *port* a camera sits
   in, which is a different question (see D5).

   **`mode: "all"` is unaffected, because it performs no *claim* join.**
   It claims every camera the SDK enumerates, so it needs no port→SDK
   mapping to decide ownership and cannot be blocked by an ambiguous one.
   That is not an exception carved out for this topology; it falls out of
   what `all` means.

   **Identity in that topology is a different matter, and `all` does not
   escape it.** D4.6 wants `noserial-{port}` for serial-less ZWO/SVBony
   cameras, but with two identical serial-less cameras nothing associates
   SDK index 0 or 1 with either port — the same absence that makes the
   claim join unresolvable. So under `all` those two cameras **keep
   today's `noserial-{index}` identity**, explicitly and with a logged
   warning naming the known weakness: reorder them on the bus and they
   swap identities and overrides. `all` keeps them *working*, which is
   its job; it cannot make them *stable*, and leaving that undefined was
   the gap. A rig needing stable identity for identical cameras
   separates the models or gives them serials.

   **But "no claim join" is not "never reads the inventory", and the
   distinction matters for identity.** D4.6 gives serial-less cameras a
   `noserial-{port}` identity, which under the permanent default would
   otherwise be underivable — `all` would have only SDK index and model.
   So the two uses of the inventory are separated, with different failure
   rules:

   - **Claiming** (`include`/`exclude`) needs a complete, trustworthy
     inventory and **fails closed** without one (D4.4).
   - **Identity minting** (all modes) is a **best-effort lookup that is
     persisted after the first success**, which is what keeps it from
     contradicting D4.6. Recomputing the identity on every start would
     make a transient scan failure publish a *different* `UniqueID` and
     orphan the camera's `devices` override — the precise instability
     D4.6 exists to remove. So: on first successful determination the
     port-derived identity must be **persisted and reused**, so a later
     scan failure cannot republish a different one.

     **The existing mechanism cannot do this, and citing it was wrong.**
     `materialize_identity` fills *fixed JSON pointers* with UUIDs, and
     all three camera services deliberately pass `&[]` for those pointers
     — `main.rs` says so outright: *"enumeration, not minted into config
     (see the design doc 'Device identity')"*. There is no per-device
     persisted identity in these drivers today, and a mechanism keyed on
     fixed pointers cannot express one keyed on a device discovered at
     runtime. So C5 either **adds a per-device identity store** (a small
     persisted map from a durable device key to its minted identity, with
     a migration for existing `noserial-{index}` entries and their
     `devices` overrides) or **drops the persistence promise** and
     accepts that a serial-less camera's identity follows whatever the
     current scan yields. The first is more work than D4.6 implied; the
     second means D4.6 buys less than it claims. Naming the choice is
     C5's job — this plan should not pretend the mechanism already
     exists. Identity must never prevent a
     camera from registering — an unstable identity is bad, no camera is
     worse, and `all` is the default that must keep working on a host
     whose USB scan fails entirely. A rig with identical
   serial-less cameras therefore either separates the models (and can
   then use `include`/`exclude`) or stays on the default and claims
   both — with no device-number or identity guarantees beyond today's,
   per D4.6 and D4.7.
4. **An unavailable USB inventory is not an empty bus.** Every collector
   returns an empty `Vec` when its source fails — sysfs unreadable,
   `system_profiler` missing, PowerShell erroring — and an empty result
   read as "no devices" would either register nothing or, worse, invite a
   fallback that opens every SDK camera and defeats the entire ownership
   boundary. The inventory must therefore distinguish *failed* from
   *empty*, and for a **non-empty `include`/`exclude`** a failed
   inventory **fails closed**: no device is opened, the service reports
   the inventory error, and doctor names it. (`mode: "all"` and empty
   claims are unaffected, and identity lookup stays best-effort — the
   full rule is below. Stating the blanket version here and the exception
   later invited implementers to pick either.)

   **A collector that never returns is a third state, and today two of
   them can hit it.** The macOS and Windows collectors shell out with a
   bare `Command::output()` — no deadline — so a wedged `system_profiler`
   or `powershell.exe` blocks startup and reload indefinitely rather than
   producing the failed-inventory result this rule depends on. "Fails
   closed" is not much comfort if the service simply never finishes
   starting. C1 therefore bounds both invocations with a timeout, kills
   the child on expiry, and maps that to the same failed-inventory state
   as a non-zero exit.

   Two things this needs beyond a signature change, both C1's:

   - **The shared boundary must carry the failure.** `facts::gather`
     returns `HardwareFacts` and folds every collector failure into an
     empty `usb` vector, so making the collectors return `Result` changes
     nothing until `gather` and its startup/doctor consumers propagate
     it. Otherwise a failed scan still reads as an empty bus.
   - **A partial scan is a failure too — but only over *candidate*
     records.** The collectors legitimately skip a great deal: the Linux
     walk passes over interface and root-hub entries that have no
     `idVendor` at all, and the macOS tree contains non-device nodes.
     Treating every skipped entry as a failure would fail closed on every
     healthy host, which is worse than the defect it guards against. So
     C1 defines what a **candidate device record** is (an entry that
     presents as a USB device: it has a vendor id, or its platform
     equivalent) and fails only when a *candidate* cannot be read or
     parsed. Non-candidates are skipped as they are today, silently.
     `Ok(empty)` still means a genuinely empty bus.

   **The rule binds only what needs a trustworthy inventory.** Stated
   once, precisely, because three earlier passes left it ambiguous:

   - **Non-empty `include`/`exclude`** require a complete scan and
     **fail closed** without one.
   - **`mode: "all"`, and `include`/`exclude` with empty lists**, never
     consult the inventory to decide *ownership* and therefore start
     regardless of scan health.
   - **All modes** may consult it **best-effort for identity** (above),
     which can never block a start and is persisted once determined.

   So a failed inventory does not stop the default configuration from
   starting. That also settles the
   **simulation backends**: `qhy-camera`, `zwo-camera` and
   `svbony-camera` built with their `simulation` feature fabricate
   cameras (`QHY178M-Simulated`, `SIM_SERIAL`) that no host USB scan can
   see, and the BDD and ConformU binaries depend on those being
   registered. Under `all` — their configuration — nothing changes. For
   a simulation build that wants to exercise `include`/`exclude`, C1–C4
   inject a **synthetic inventory** alongside the synthetic cameras
   rather than bypassing the filter, so the tests exercise the real join
   rather than a special case around it.
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
   reordering and replug — **in `zwo-camera` and `svbony-camera`, the two
   drivers that mint such a fallback**. QHY mints none (its raw SDK id is
   the identity), so its equivalent is a C5 decision, not this change;
   see the paragraph after D4.7. Small, obviously right where it applies,
   and it closes a documented ambiguity — but it **changes `UniqueID` for affected cameras**, so it
   lands with C5 and its documentation, not silently inside a driver
   phase (see the breaking-change note after D4.7).
7. **`device_number` stability — for explicit claims only.** Alpaca
   device numbers are assigned by enumeration order today, so unplugging
   one camera renumbers the others and silently re-points every
   `cameras[].device_number` in rp's config. **Under `mode: "include"`
   only**, assign by the claim's **position in `usb_ports`**, which is
   the operator's declaration and does not move when a camera is absent.
   That would require device numbers to be **sparse and never
   compacted** — a claim of `["1-4.2", "1-4.3"]` with the first camera
   unplugged leaving the survivor at device number 1, with 0 simply
   absent that night.

   **The pinned `ascom-alpaca-rs` cannot express that, so this is an
   open implementation decision rather than a promise.** Devices are held
   in a `Vec` per device kind and `iter_all()` assigns numbers with
   `.enumerate()` (`src/api/macros.rs`), with request lookup indexing the
   same `Vec` — so registering only the survivor exposes it as 0, not 1,
   and it shifts back when the absent camera returns. The protocol allows
   a gap; this server implementation does not offer one. C5 picks one of:

   1. **Bind by `UniqueID` instead of `device_number` in rp** — the only
      option that makes the binding immune to both absence and
      reordering, and the reason the identity work in D4.6 matters. The
      largest change: `cameras[].device_number` becomes a resolved
      detail rather than the configured key.
   2. **Register a placeholder device for an absent claim** so positions
      hold. Cheap, but it puts a permanently-disconnected device in the
      roster that ConformU and clients will see.
   3. **Upstream explicit device numbering** into `ascom-alpaca-rs`.
      Cleanest protocol-wise, slowest, and outside this plan's control.

   Until one is chosen, **no mode offers a device-number stability
   guarantee**, and the plan says so rather than implying otherwise.

   **`exclude` and `all` get no stability guarantee**, and the reason is
   structural rather than an omission: `exclude`'s `usb_ports` names the
   devices to *omit*, so it declares no ordering for the survivors at
   all, and any fallback to enumeration or sort order renumbers a later
   camera when an earlier one is unplugged. A multi-camera rig that wants
   stable device numbers will use `include` **once C5 picks a mechanism
   that can deliver them** — the positional rule above is the desired
   design, not a commitment, and nothing should be implemented against it
   until then. Under `mode: "all"` sorting by port is only a
   partial fix and the plan should not claim otherwise: with cameras at
   A/B/C, removing B still moves C from 2 to 1. **`mode: "all"` therefore
   offers no device-number stability guarantee either**, and a multi-camera rig
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
`noserial-{port}` — **in the two drivers that mint such a fallback**.
In `zwo-camera` and `svbony-camera` the minted string is *both* the
`UniqueID` suffix **and** the key of the `devices` override map
(`mint_identity` in each), so the change moves both: an existing
`devices` entry keyed `noserial-0` must be re-keyed to
`noserial-1-4.2`, and the `UniqueID` such a camera publishes changes
with it.

**QHY is different and needs its own decision.** It mints nothing: the
raw `GetQHYCCDId` string *is* the `UniqueID` and the `devices` key
(`qhy-camera/src/camera.rs`). For models whose id carries a constant or
absent serial suffix, two such cameras therefore collide on `UniqueID`
today — a pre-existing defect this plan exposes rather than causes. C5
must decide whether QHY gains a port-based identity for that case (and
carry the matching `devices`-key and shared-CFW-id migration) or whether
the collision is documented and left alone. It cannot be swept under
"same change for the other drivers", which is what this plan previously
implied.

**Both breaking changes (D4.6 — serial-less identity, and D4.7 —
device-number assignment) land together in C5, now.** The
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
service's existing `doctor` subcommand grows a device listing, printed as
claim-ready rows.

This is **new plumbing, not a reuse**: the per-service probe today calls
`Sdk::new()` and the shared runner emits only config and SDK checks — it
never receives the passive `HardwareFacts` USB inventory, which is
gathered on the central-doctor path. Since `--devices` must print ports
*before* any SDK device-touching operation, C1/C5 add the passive
collector (and its error handling, per D4.4) to the per-service path
rather than extending the existing SDK listing.

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
never "this camera has no identity".

**The SDK id column is SDK-specific, for the same reason.** QHY's
`GetQHYCCDId` is available during passive enumeration, so it can be shown
for every QHY device including excluded ones. **ZWO's is never shown** —
not even for a claimed camera. `doctor --devices` is a separate,
short-lived process with no open handle of its own, so populating that
column would mean `ASIOpenCamera` from inside the setup tool, against a
camera the running service may be streaming. Claimed or not makes no
difference to that; the earlier wording ("shown only for cameras the
driver claims") promised data the tool cannot produce without breaking
its own contract. The rule is "print what this SDK yields passively" —
never open anything, and never hide an id that was free to read.

Two checks join the per-service set, alongside `config.full-shape` and
`hardware.sdk-devices`:

| Check | Trigger |
|---|---|
| `claims.resolve` | A claimed port holds no device (`warn` — absent hardware or a moved cable), or holds one that cannot be joined to an SDK device (`fail` — the driver will register nothing for it). |
| `claims.unclaimed` | Devices on the bus that no claim covers, listed for information (`ok`) — so "why is my camera missing" answers itself. |
| `claims.implicit` | The driver is on `mode: "all"` **and** enumerated more than one device: names the devices it just claimed and prints the paste-ready `include` block for claiming a subset (`ok`, informational). Silent on a single-device rig, where nothing is ambiguous. **It must not print a block that cannot work:** on a rig matching D4.3's unresolvable topology (identical models with no passive join signal — for ZWO, identical models at all), an `include` claim would be syntactically valid and still register neither camera, so the check reports the collision and the reason instead of a suggestion. Suggesting a claim that fails is worse than suggesting nothing. The same applies when the **USB inventory itself is unavailable**: `mode: "all"` starts regardless (D4.4), but with no scan there are no port paths to print, so the check reports the unavailable inventory and suppresses the suggestion rather than emitting a block with nothing in it. |

An automated *drift* check ("this port used to hold a QHY268M") is
deliberately **not** in scope: with the port as the only key there is
nothing in the config to compare against, and inventing a remembered-state
file to enable one check is not worth it. The listing above shows the
operator what is where; a moved cable shows up as a `claims.resolve` warn
plus an unfamiliar model in the table.

**For a camera shared with PHD2, though, that is not good enough, and
`exclude` is the wrong mode.** Exclusion is negative: if the excluded
camera is moved to a different port, its new port is simply an unlisted
survivor and the driver **opens it** — recreating the exact ownership
conflict this plan exists to prevent, with a `claims.resolve` warning
that arrives after the damage. `include` has the opposite failure mode:
a moved camera stops being claimed, which is safe. So the guidance is
explicit — **a camera shared with another application is protected with
`include` on the cameras you own, not `exclude` on the one you don't** —
and the earlier `qhy-camera.json` example is the illustration of a
mechanism, not a recommendation for the guide camera.

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

**Stage 2 is still not safe at startup, and this is the sharpest thing
in Part A.** Filtering to claimed cameras stops the probe reaching
*other people's* devices, but it does not make the probe itself
permissible: `InitQHYCCD` is what may auto-home a connected CFW, and
[workspace tenet 3](../workspace.md#project-tenets) forbids any code path
reachable from **service startup, reconnect, or config apply** from
physically actuating hardware. A filter changes who gets actuated, not
whether actuation happens on a passive transition. `qhy-camera.md` only
ever documented that SDK side effect on an **explicit client connect**,
which is a different trigger entirely.

So C4's scope is larger than a filter: the CFW-plugged probe must leave
startup altogether. Options, to be settled in C4's design phase:

1. **Defer the probe to client connect** — the trigger where the effect
   is already documented and where an operator has asked for the device.
   The filter-wheel device would then be registered lazily, or
   registered always and report its presence on connect.
2. **Find a non-actuating detection path** — if any SDK call can report
   a plugged CFW without a full `InitQHYCCD`, prefer it. The reference
   driver's sequence suggests there is not one, so this needs checking
   rather than assuming.
3. **An explicitly operator-started probe** — but **not** via the two
   paths a previous draft of this bullet named, because both forbid it:
   `config.apply` is inside tenet 3's no-actuation set
   ([workspace.md](../workspace.md#project-tenets)), and doctor's
   hardware probes are read-only and *never* an open
   ([doctor.md](../services/doctor.md)). Proposing either would have had
   C4 violating a contract while trying to satisfy one. If this option is
   taken it needs a **distinct, explicitly-unsafe operator command** that
   says what it will actuate before it does, with its own amended
   contract — not a quiet addition to an existing read-only surface.

Until one is chosen, the enumerate/probe split alone does **not**
discharge the tenet-3 problem — it narrows it. Recording that plainly,
because the split was introduced in this plan as though it did.

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
    "capture_grace": "10s",
    "server": { "port": 11128 }
  }
  ```

  **Being an Alpaca device, the facade owes the shared configuration
  contract too.** [`config-actions.md`](../services/config-actions.md)
  requires every driver to expose `config.get`, `config.apply` and
  `config.schema`; without them `ui-htmx` cannot edit `enabled`, the
  pixel sizes or `image_dir`, and a change to the listener has no defined
  reload-or-restart behaviour. C6 implements the three Actions on the
  facade device and declares its disposition — and the listener fields
  (`enabled`, `server.port`) are **restart-only**, since rebinding a
  socket under live clients is not a reload. That runs into the same
  per-driver `ApplyDisposition` limit as D4.1's `claims`, so the two
  share one decision rather than inventing two answers.

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
  C6 therefore includes: the workspace index row, the port table row in
  `packaging.md`, the `.wxs` firewall exception, the Linux packaging
  notes, and the **other shipped registries that hard-code 11130** —
  `docs/packaging-windows.md`, `installer/Package.wxs`, and the
  `scripts/check-pkg-assets.sh`, `verify-packages.sh`, `verify-brew.sh`
  and `verify-msi.ps1` reachability checks, each of which needs the
  facade's *optional* nature expressed (a port that exists only when
  `camera.enabled` is true, not a second unconditional one) — and **a catalog schema that can express an optional second
  listener**, which today it cannot: `CatalogEntry` carries one `class`
  and one `default_port`, so a second unconditional port would raise
  false collision and availability findings whenever `camera.enabled` is
  false, while changing `class` would break probing of the existing core
  endpoint at 11130. C6 defines the multi/optional-listener catalog shape
  and the check semantics that go with it — or, if that is judged too much for one
  phase, an explicit decision to bind the facade loopback-only and say so
  in the design doc.
- **Opt-in.** `camera.enabled` defaults to `false`. An unrequested second
  Camera device in the roster is confusing, and enabling it costs PHD2
  round trips at startup.
- **The facade needs its own `UniqueID`, and the workspace already has
  the mechanism.** `crates/rusty-photon-config` exists because *"ASCOM
  Alpaca requires every device's `UniqueID` to be globally unique and to
  never change, but the protocol enforces neither"*: it mints a UUIDv4
  per device on first run, persists it atomically, and never overwrites
  an existing id.

  **Adopting it changes `phd2-guider`'s package lifecycle, which C6 must
  handle rather than inherit by accident.** The service passes `&[]` for
  identity pointers today (`main.rs`) — but note what is *not* true: bare
  `phd2-guider serve` already calls `resolve_and_init` and materializes a
  default config (`main.rs`, and `phd2-guider.md` § "Config-path
  resolution and first-start creation"). Only the **verification
  scripts'** classification says otherwise, and that classification is
  stale. So this is not the transition from "no config file" to "one";
  it is adding a persisted UUID to a file that already gets written. C6
  therefore either materializes **only when `camera.enabled` is true**,
  or reconciles that stale verification expectation — a smaller job than
  the earlier wording implied, but still a deliberate one. The facade uses
  `materialize_identity` at its own config pointer like every other
  driver — **not** a value derived from
  PHD2's profile, the guide camera's model, or the port, all of which
  change when the operator reconfigures PHD2 and would silently
  re-identify the device to every client that stored it. C6 settles this
  before C7 wires the device into rp.
- **`StartExposure(duration, light = false)` is rejected.**
  `capture_single_frame` has no dark-frame mode, and PHD2 will not close
  a shutter the guide camera does not have — so accepting `light = false`
  and returning an ordinary light frame would silently violate the ASCOM
  Camera contract and hand a calibration pipeline a mislabelled frame.
  The facade returns `InvalidValueException`. **This is a
  facade-specific contract, not a repo precedent** — an earlier draft of
  this plan cited `sky-survey-camera` as already rejecting dark frames,
  which is wrong: it *accepts* `light == false` and returns a zero-filled
  frame (`services/sky-survey-camera/src/camera.rs`). That is a defensible
  choice for a synthetic sky source and the wrong one here, where a
  zero-filled frame would be indistinguishable from a real dark to a
  calibration pipeline and PHD2 has no shutter to close. C6 covers the
  rejection with a BDD scenario.
- **Exposure, and it is asynchronous.** `StartExposure(duration,
  light = true)` schedules the capture and **returns** — it does not run
  the sequence inline. A client polls `ImageReady`, which the background
  task alone publishes, exactly as the repo's other camera drivers
  behave; blocking the Alpaca request through a watermark wait, a FITS
  read and a decode would stall the caller for the whole exposure and
  break the poll contract ASCOM clients rely on. The background task
  does: `capture_single_frame` **with its own `exposure` parameter** →
  wait for the D9
  watermark → `save_image` → validate the path → read → decode to the
  `ImageArray` cache → **delete** the file, publishing `ImageReady` only
  at the end.
- **The capture must not change PHD2's guiding exposure.**
  `set_exposure` mutates PHD2's *global* setting, so an autofocus frame
  at 4 s would silently leave the next guide loop running at 4 s — a
  configuration change disguised as a capture. `capture_single_frame`
  already accepts a per-call `exposure` (D9), and the existing
  `Phd2Client` passes it, so the facade uses that and never touches the
  global. If some future path must set it, it restores the previous value
  under the same arbitration.
- **The wait is bounded — by the *effective* exposure, not the
  requested one.** If C6 chooses to snap an unsupported duration to the
  nearest value PHD2 supports (the exposure contract below), the
  effective exposure can be **longer** than the request, and a deadline
  computed from the request would expire while PHD2 is legitimately
  still exposing — dropping a valid capture into the unknown-state
  quarantine. So the deadline is `effective exposure + capture_grace`;
  if C6 instead rejects unsupported durations outright, the two are the
  same number and nothing changes.
  `camera.capture_grace` is a humantime duration like the rest of the
  config tree, **default
  `10s`**, rejected at load if zero or above a sane ceiling (`60s`): it
  covers PHD2's download and write of one frame, not an arbitrary wait,
  and a grace longer than the ceiling is a misconfiguration rather than
  patience. After it elapses the exposure fails with a
  structured error and `ImageReady` stays false. Without a deadline a
  wedged PHD2 or a missed watermark parks the exposure forever.

  **The existing client cannot enforce that deadline by itself.**
  `Phd2Client` wraps every RPC in its own fixed `command_timeout`
  (`config.rs`, default **30 s**), so a PHD2 that stops replying on a
  single watermark poll or on `save_image` blocks for up to 30 s per
  call — well past a `capture_grace` measured in seconds, while holding
  capture-arbitration. C6 therefore threads the **remaining** capture
  deadline into each await (or applies a per-operation timeout derived
  from it), rather than assuming the sum of the client's own timeouts
  respects the capture's budget. The two budgets are independent today
  and must be made to compose.

  **The deadline does not release capture-arbitration**, and this bullet
  deliberately does not say otherwise — see D11. A missed watermark means
  the facade does not know whether PHD2 is still exposing, so arbitration
  is held in a quarantined state and conflicting operations keep being
  refused with a structured unknown-state error until the watermark
  finally arrives or the operator intervenes. Releasing on a timeout is
  precisely how guiding would start on top of a live exposure.
- **File cleanup is a guard, not a happy-path step.** Deletion runs on
  every exit from the capture — success, read error, decode error,
  timeout, cancellation — not only after a successful decode. An
  overnight sweep that fails at the decode step must not leave a FITS per
  attempt on PHD2's host. A failed deletion is logged, never fatal.

  **One exit cannot be covered by a guard, and needs a different
  mechanism.** If `save_image` writes the FITS but its RPC times out or
  the connection drops before returning the filename, the facade never
  learns the name — there is nothing to hand the guard, and the path
  rules below forbid guessing. Those orphans accumulate silently over a
  night of failures. So C6 adds a **reconciliation sweep** over
  `image_dir`: on startup and after any uncertain `save_image`, delete
  files older than a retention window that the facade did not
  successfully hand back, logging what it removes. `image_dir` is
  exclusively the facade's working area by construction, which is what
  makes an age-based sweep safe there and would make it reckless
  anywhere else.

  Two things keep that sweep honest:

  - **An age cutoff is not a work bound.** `read_dir` still enumerates
    every entry, so a directory that accumulated thousands of orphans
    makes startup arbitrarily slow. The sweep takes an explicit entry (or
    time) budget, processes what it can, and reports what it left — a
    slow startup is a worse failure than a late cleanup.
  - **The orphan this scenario creates is younger than the cutoff.** A
    timed-out `save_image` can land *after* the sweep runs, so the file
    it wrote is newly created and survives every age-based pass until the
    next restart — or forever on a service that stays up for months. So
    an uncertain `save_image` also arms a **bounded follow-up** at
    roughly the retention window, which sweeps once more for exactly
    that case.
- **The returned path is validated before it is touched — and the
  earlier wording of this rule would have broken every exposure.**
  `save_image` returns a **full path** (D9), so requiring the returned
  value itself to contain no separators, as this plan previously did,
  rejects every normal absolute path and every Windows drive path: no
  frame would ever be read. The rule is in two parts, and the order
  matters:

  1. **Lexically** verify the returned path is inside `camera.image_dir`
     — after normalizing `.` and `..` textually, without touching the
     filesystem — and take the **relative remainder**, which must be a
     single component. Not `canonicalize`: that resolves symlinks, which
     is both a filesystem round-trip and exactly the wrong check, since
     it would resolve a planted link to a target outside the directory
     and report success.
  2. **Open that single component** relative to a dirfd for `image_dir`
     with `O_NOFOLLOW`, then `fstat` the resulting handle to confirm it
     is a regular file. The open and the check are then the same object,
     which is the property a preflight check cannot have.

  Anything failing either part is refused with a structured error and
  nothing is read or deleted.
  PHD2 is a local trusted process in the normal case, but "reads and
  deletes an arbitrary path a peer names" is not a property to leave
  unbounded in a service running as its own user.

  **Validate-then-act is a TOCTOU window, so the check and the use must
  be the same handle.** `image_dir` is shared with another account by
  construction (above), so a path can be swapped between the check and
  the read. C6 uses no-follow, directory-handle-relative operations —
  `openat`-style with `O_NOFOLLOW` against a dirfd for `image_dir`,
  rather than a preflight `canonicalize` followed by a bare `read`.

  **The dirfd closes the read window but not the unlink one**, and
  claiming otherwise was imprecise: `openat` binds the *read* to one
  inode, but `unlinkat(dirfd, name)` resolves the name again, so a peer
  that replaces the file between read and delete gets a different inode
  unlinked. C6 settles this explicitly rather than leaving it implied —
  either an ownership check against the opened handle's inode
  immediately before the unlink (racy in principle, adequate against
  accident rather than attack), or a stated **trusted-peer assumption**:
  `image_dir` is shared with exactly one other local process, PHD2, run
  by the operator themselves, and a hostile process with write access
  there has already lost the operator more than a FITS file. The
  assumption is defensible; leaving it unstated while implying the
  window is closed is not.

  **The returned value is required to be a direct child of `image_dir`**
  — a single path component, no separators, no `..` — which is what
  makes the dirfd approach complete rather than merely careful. Nested
  components would each need beneath-directory, no-follow traversal to be
  safe against a peer replacing an intermediate directory between
  operations; refusing them costs nothing (PHD2 writes into the directory
  it is told to) and removes the whole class. The Windows equivalent
  (`FILE_FLAG_OPEN_REPARSE_POINT` + handle-relative delete, or the
  `std::fs` no-follow primitives where available) is specified alongside,
  since the facade is cross-platform.
- **The read and decode are bounded too.** The capture deadline covers
  the watermark wait *and* the read/decode, and the file is size-capped
  from the advertised frame geometry plus FITS header overhead: an
  oversized or malformed file in a shared directory must not cause an
  unbounded allocation, and must not hold the arbitration state while it
  is chewed through — that starves guiding for as long as the decode
  runs.
- **Capability surface.** `CanAbortExposure`/`CanStopExposure` `false`
  (PHD2 offers no cancel for a single frame), no cooler control, no
  gain/offset (PHD2 owns those through its equipment profile), bin 1 only.
  `CameraXSize`/`CameraYSize` from `get_camera_frame_size`. `MaxADU`
  65535.
- **The exposure range and resolution are part of the contract and were
  missing.** ASCOM requires `ExposureMin`, `ExposureMax` and
  `ExposureResolution`, and without them an Alpaca client may request any
  representable duration — leaving `requested exposure + capture_grace`
  with no meaningful upper bound and the seconds→milliseconds conversion
  undefined. PHD2 exposes a **discrete** set via `get_exposure_durations`,
  not a range, so C6 defines the mapping explicitly: `ExposureMin`/`Max`
  from the ends of that list, `ExposureResolution` reflecting its
  granularity, and a requested duration that is not in the list either
  snapped to the nearest supported value or rejected with
  `InvalidValueException` — decided in C6 and stated, not left to the
  implementation. The list is read once PHD2 is reachable and cached like
  the geometry properties.
- **The geometry properties need a contract for "PHD2 is down".**
  `ServerBuilder` deliberately binds and serves while PHD2 is
  unreachable — `/health` reports 503 and a background task retries — so
  the Alpaca listener will be answering property reads before any
  successful `get_camera_frame_size`. Blocking startup until PHD2
  answers would break that documented lifecycle; reporting zeros or
  guesses would break ASCOM and poison rp's train optics. C6 specifies
  the third option: the properties are **read lazily and cached on first
  success**, and until then they return an ASCOM error (the
  not-connected/unavailable mapping), with `Connected = true` requiring a
  live PHD2 session so a client's first move surfaces the real state. Passing ConformU with a surface this narrow is a real work item,
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
  `/var/lib/rusty-photon/phd2-images`, already inside the unit's existing
  `ReadWritePaths=/var/lib/rusty-photon`), with PHD2 configured to save
  there.

  **`ReadWritePaths=` only grants the *service* access — it grants the
  operator's account nothing**, and PHD2 runs as that operator under
  their VNC session, so without a second grant the very first
  `save_image` fails on write. C6 must specify the sharing concretely and
  test it.

  **`0775` + `g+s` is not sufficient on its own**, and naming it as the
  recipe was wrong: `g+s` makes new files inherit the directory's
  *group*, but not a readable *mode* — an interactive PHD2 running under
  `umask 077` creates a `0600` FITS owned by the operator, correctly
  group-`rusty-photon`, and still unreadable and undeletable by the
  service. The sharing therefore needs a **POSIX default ACL** on
  `image_dir` (`setfacl -d -m g:rusty-photon:rwx`), which does set the
  mode on new files — **but a default ACL alone still only describes the
  service's group.** An operator outside `rusty-photon` cannot traverse
  or create in the directory at all, so the acceptance test fails before
  the service ever reads anything. This is the third pass over this
  recipe and the lesson is that both sides need granting explicitly: an
  access ACL admitting the **operator** (`setfacl -m u:<operator>:rwx`,
  or their group) *and* a default ACL giving the **service** group `rwx`
  on new files (`setfacl -d -m g:rusty-photon:rwx`). Alternatives remain
  a documented `umask` for the PHD2 session, or a deliberately same-user
  deployment. Whichever is chosen goes into
  `packaging.md`'s PHD2 section as a step, not an aside, and C6's
  acceptance test is concrete: **a file created by the operator's PHD2 is
  read and deleted by the service account.**

  **Windows needs its own recipe, not a translation of this one.**
  `ReadWritePaths=`, group bits and POSIX ACLs mean nothing there, and
  the MSI installs the service as **`LocalSystem`**
  (`installer/fragments/phd2-guider.wxs`) while PHD2 runs in the
  operator's interactive session — so the same read/write/delete failure
  appears on Windows for entirely different reasons. C6 specifies a
  Windows `image_dir` (under `%PROGRAMDATA%`, matching where the
  machine-wide config already lives) and the ACL grant that lets both the
  service account and the interactive user read, write and delete there
  — or deliberately requires same-user execution on Windows and says so.
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
  why.

  **Reusing `op_lock` with `try_lock` cannot express this, and saying so
  was still too loose.** `op_lock` is one mutex that every guiding
  mutation takes with `lock().await`; switching `guiding/start` to
  `try_lock` would make it fail behind an ordinary dither — breaking the
  guiding-to-guiding queue this plan promises to leave alone — while
  leaving it as `lock().await` makes it queue behind an exposure, which
  is the thing being fixed. One lock cannot have two disciplines.

  C6 therefore adds a **separate capture-arbitration state** beside
  `op_lock`, with an explicit acquisition order and three rules:

  1. **Capture** takes capture-arbitration with `try_lock`, then
     `op_lock`. Busy on either → immediate structured `busy` error
     naming the operation in flight.
  2. **Every non-privileged PHD2 mutation** — `guiding/start`, and
     equally `pause`, `resume` and `dither`, all of which the service
     exposes and `op_lock` already serializes, **and equally
     `clear_calibration` and `reselect_star`** (the latter drives the
     guide camera directly) — keeps `lock().await` on `op_lock` (queueing
     behind guiding operations, unchanged) but is refused while a capture
     holds capture-arbitration. C6 enumerates every mutating endpoint and
     internal client path rather than listing examples, with a scenario
     per operation that can reach the camera. Naming only
     `guiding/start` earlier was an oversight: a `resume` reaching PHD2
     while `capture_single_frame` holds the guide camera violates the
     same exclusivity contract, and the rule is about *which device is in
     use*, not about which endpoint was called.
  3. **Privileged stop** takes neither in the blocking sense — see the
     next bullet.

  Always capture-arbitration before `op_lock`, never the reverse, so the
  two cannot deadlock. Each rule gets a BDD scenario.

  **Two locks and a check are not enough, and rule 2 as stated has a
  race.** `guiding/start` can observe capture-arbitration free, then
  block on `op_lock`; a capture can take both in that window; `start`
  then wakes holding `op_lock` *behind* the exposure — queued, which is
  exactly the outcome the rule promises to prevent, with no recheck to
  catch it. A check that is not atomic with the acquisition it guards is
  not a guard. C6 therefore owns this as a named design obligation:
  either a **single coordination state machine** for the three paths
  (capture / guiding-mutation / privileged stop), or an **atomic
  reservation with a recheck after `op_lock` is acquired** that converts
  a lost race into the same structured `busy` error. Ordinary
  guiding-to-guiding queueing must survive whichever is chosen, and the
  race itself gets a BDD scenario — it is reproducible with two
  concurrent requests.
- **Stop is privileged — and that needs a cancellation path, not just a
  lock bypass.** `guiding/stop` and the safety path must never be refused
  because an exposure holds the arbitration state. But `GuiderOps::stop`
  takes the same mutex today, so "stop bypasses the lock" alone would
  leave the capture running on into `save_image`, the decode and the
  cleanup *after* the stop returned — the operator believes the rig is
  stopped while a PHD2 exposure is still in flight. C6 therefore
  specifies an **independent privileged stop path** with explicit
  cancellation and join semantics: stop signals the in-flight capture to
  cancel, the capture observes the signal at its await points, and its
  cleanup guard still runs.

  **But cancelling our future does not cancel PHD2's exposure** — D9
  records that PHD2 offers no cancel for a single frame, which is why
  `CanAbortExposure` is `false`. So unwinding the local task and then
  releasing capture-arbitration would let `guiding/start` proceed while
  the guide camera is *still physically exposing*, with stop having
  reported success. The arbitration state must therefore outlive the
  local future: stop releases it only after the D9 completion watermark
  is observed (the exposure genuinely finished), or, if the bounded wait
  elapses first, **reports a bounded failure and leaves the conflicting
  operations refused** rather than releasing into an unknown camera
  state. The stop itself still returns promptly — what waits is the
  right to start guiding, which is the thing that would actually collide.

  **That means stop has two halves, and the plan should name them rather
  than claim both "joins" and "returns promptly".** The foreground half
  acknowledges the stop: it signals cancellation, stops the guide loop,
  and returns — it does *not* wait for PHD2's exposure, which it cannot
  cancel. The background half is a **detached quarantine/drain task**
  that owns the capture's cleanup guard, waits for the D9 watermark or
  the bounded deadline, and only then releases capture-arbitration.
  Conflicting operations are refused, with the structured unknown-state
  error, for as long as that task holds it. So: stop joins its own
  cancellation, not the exposure; nothing is detached without an owner;
  and no caller is told the camera is idle while it may still be
  exposing.

  **An in-memory task is not a lifetime guarantee, though.** The service
  exits its serve path as soon as shutdown is signalled and the unit
  restarts it on failure — so a restart between `capture_single_frame`
  and the watermark simply drops the quarantine, and the next process
  starts with a clean slate and will happily accept `guiding/start` while
  PHD2 may still be exposing. C6 therefore specifies both ends: a
  **shutdown drain** that lets an outstanding quarantine finish (bounded,
  like every other shutdown step), and a recovery rule for when the drain
  does not get to run.

  **That recovery cannot be a state query**, and proposing `get_app_state`
  for it contradicted this plan's own D9: `Stopped` is true before,
  during *and* after `capture_single_frame`, so a fresh process asking
  PHD2 what it is doing gets an answer that cannot distinguish "idle"
  from "exposing". The new process has no pre-capture watermark to
  compare against, because the watermark lived in the process that died.

  So the facade **persists a capture generation marker** alongside its
  identity — written before `capture_single_frame`, cleared when the
  watermark is observed — and on startup, finding an uncleared marker, it
  **starts fail-closed**: capture-arbitration is held and conflicting
  operations are refused with the unknown-state error until the recorded
  capture is known to have finished.

  **That recovery only exists for one of D9's three candidate
  watermarks.** A frame counter can be compared against a persisted
  value across a restart; an observed *transition* cannot (the
  observation died with the process), and `DATE-OBS` only helps if a
  frame actually landed. So the watermark choice and the crash-recovery
  story are the same decision, not two: **if C6's measurement makes the
  frame counter workable, it is mandatory**, precisely because it is the
  only one that survives a restart. If it is not workable, C6 must define
  a safe recovery for whichever fallback it picks — bounded, and never
  "the operator clears it by hand", which is not a recovery procedure for
  an unattended rig at 3am.
  Fail-closed after an unclean restart costs a delayed guide start; the
  alternative costs a guide loop driven onto a live exposure, which
  nothing downstream can recover.

### D12. What this changes in rp (C7)

The guide camera becomes an ordinary `cameras[]` entry — `alpaca_url`
pointing at the facade, `device_number: 0` — and therefore a legal terminal
camera of the guiding train. Then:

- **The operation selected in C7's focus-model reconciliation** (below —
  deliberately not `auto_focus`, which is being retired) can run the
  **ordinary capture sweep** (`move_focuser` + `capture` +
  `measure_basic`) against the guiding train, with the precondition
  *PHD2 in `Stopped`* — not merely "not guiding". Per D11 a
  looping or partially-paused PHD2 refuses the capture, so the sweep would
  fail at the first frame.

  **A single `get_app_state` read is a snapshot, not a reservation**, and
  a focus sweep is many frames with focuser motion between them. Another
  PHD2 client — or rp's own `guiding/start` — can leave `Stopped` between
  samples, so per-exposure `try_lock` would either fail mid-sweep after
  the focuser has already moved, or let guiding start between samples and
  silently change what the later frames measure. C7 therefore needs the
  facade to expose a **sweep lease**: the capture path is reserved for
  the whole sweep (acquired before the first move, released in a guard
  after the last frame or on failure), with `guiding/start` refused for
  its duration and the privileged stop path still able to break it. rp
  does **not** stop guiding to acquire the lease: that is the operator's
  or the workflow's call.

  **The lease needs an API, and the ASCOM Camera surface has none** —
  which the plan previously glossed over. As written there is no call rp
  can make to take the lease before the first focuser move, so the first
  `StartExposure` would fail only *after* hardware had already moved.
  C6 defines the companion interface and C7 consumes it; the options, to
  be settled in C6's design phase:

  - a pair of **ASCOM `Action`s** on the facade device
    (`rustyphoton:sweep-lease-acquire` / `-release`), which keeps
    everything on the one Alpaca device rp already talks to and is the
    conventional escape hatch for vendor-specific operations;
  - **REST endpoints on the guider service** (`POST
    /api/v1/capture/lease`), consistent with the rest of the
    rp↔guider contract but splitting the camera's control across two
    transports;
  - an **rp-owned reservation** that never touches the facade, which is
    simpler but cannot refuse a `guiding/start` arriving from anywhere
    else.

  The lease also carries the `Stopped` precondition check, so rp learns
  before moving the focuser rather than at the first frame.

  **A lease held across processes needs an owner and a liveness rule**,
  which a guard on rp's side cannot supply: if rp crashes or its
  transport drops after acquiring, the guard never runs and the facade
  refuses guiding for the rest of the night with no request completion
  to release it. The privileged stop path is an operator escape, not a
  recovery mechanism, and an unattended service has no operator at 3am.
  So C6 gives the lease: an **owner token** returned at acquisition and
  required to release; a **bounded TTL** with explicit renewal from rp
  while the sweep progresses, so a dead owner's lease expires on its own;
  and expiry semantics that still **wait for the capture watermark**
  before releasing, since a dead rp says nothing about whether PHD2 is
  still exposing. A lease that cannot expire is an outage waiting for a
  crash.

  **The facade lease is necessary but not sufficient: it says nothing
  about rp's own concurrent work on the same train.** The motion gate
  admits concurrent *shared* imaging captures, and `move_focuser` has no
  sweep-wide reservation — so during a guide sweep a main-camera exposure
  can be in flight, or another focuser move can be issued, while the
  sweep is stepping a focuser the train **shares** (the reference rig's
  EAF moves the drawtube and therefore everything behind it, which is the
  coupling `optical_trains` exists to model). The main camera would then
  record a frame taken mid-move. C7 needs an **rp-wide
  focuser/optical-operation lease** over the affected train — acquired
  before the first move, released on every completion, cancellation and
  failure path — alongside the facade lease and the mount-motion lease of
  D12. **Acquisition must drain, not merely exclude:**
  `MotionGate::shared()` permits concurrent captures and does nothing
  about one already holding a shared permit, so a main-camera exposure
  that started *before* the sweep took the lease is still in flight when
  the focuser moves. The lease needs read/write semantics — wait for
  affected captures to finish, then block new ones for the sweep — not
  just serialization of focuser moves against each other. Three leases sounds heavy; it is three different owners (PHD2, the
  mount, rp's own train operations) and each is already a demonstrated
  way to corrupt a sweep.

  **The lease is local, not atomic with PHD2**, and calling it "atomic"
  earlier overstated it. It gates requests arriving *through the facade*;
  PHD2 accepts connections from anyone, and multiple simultaneous clients
  on port 4400 is a documented property of its API (D9). Another client
  — a hand-opened PHD2 window, a second suite — can start looping
  mid-sweep and the lease cannot prevent it. So C6/C7 specify both
  halves: **sole control of PHD2 during a sweep is a deployment
  precondition**, stated in the design doc, and the **fail-safe** for
  when it is violated — each frame re-checks the allowlisted state
  (D11), and a sample taken from a PHD2 that left `Stopped` aborts the
  sweep with a structured error naming the state, rather than silently
  contributing a frame taken under different conditions to the V-curve.
- The existing **PHD2-metric sweep is kept, not replaced.** The two have
  opposite preconditions — the metric sweep needs an active guide loop, the
  capture sweep needs no guide loop — so they cover different moments:
  metric for mid-session refocus while guiding, capture for start-of-night
  focusing.

- **C7 cannot hang the capture sweep off rp's `auto_focus`, because that
  tool is being retired.** [`focus-model.md`](focus-model.md) D17 is
  explicit: S7 *"removes `rp`'s capture-based `auto_focus`,
  `refocus_train`, and the `auto_focus` block on imaging trains"*, and
  *"the PHD2-metric sweep keeps the `auto_focus` name for guiding trains
  … until O4 moves it."* So the name this plan was reaching for will mean
  the **metric** sweep, and the capture-based machinery behind it will not
  exist. Updating `rp.md` and `optical-trains.md` alone would leave the
  provider and `session-runner` contracts contradicting each other.

  C7's design phase reconciles the two plans before any code, choosing
  between:

  1. **The guide-train capture sweep lands in the `focus-model`
     provider** as a mode of `focus_train` — consistent with D17's
     direction, since that provider becomes the expert on focusing a
     train, and the facade simply gives it a camera it previously did not
     have. The likely right answer.
  2. **A separate, explicitly-named rp operation** for capture-focusing a
     guiding train, which avoids overloading `auto_focus` during its
     retirement but adds a tool the focus provider may want back later.

  Either way the sequencing matters: **C7 must not land before its
  relationship to focus-model's S7 is settled**, or the two plans will
  race on the same contract. This is a plan-level dependency, recorded
  here and worth mirroring into `focus-model.md` when C7 starts.
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
  fit a curve through trailed stars.

  **The imaging-train treatment is not sufficient on its own**, because
  `MotionGate::shared()` is held for the duration of one `capture` call
  and released before the focuser moves for the next sample — so a queued
  slew or dither runs *between* samples, changes the field, and corrupts
  the V-curve just as thoroughly. C7 must hold a **mount-motion lease
  across the entire sweep**, acquired before the first focuser move and
  released in a guard after the last frame, alongside the sweep lease of
  D12. This is a change to `imaging_permit`'s contract and to `rp.md`
  § Mount Motion Gate, not an incidental fix.
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
| 6 | **Both breaking changes land in C5, at 0.1.0** (D4.6, D4.7) | Pre-1.0, no CHANGELOG, few rigs — the cheapest this will ever be, and both fix ambiguities the code documents as flaws. One disruption, one upgrade step. |

Nothing in this plan is waiting on an *operator* answer. **Seven** things
are waiting on evidence or an implementation choice, each named at its
rule — three of them block a phase outright (C1's spike, C4's
tenet-3-safe probe path, and C7's focus-model reconciliation):

- **C1 — the Windows port spelling**, largely answered on `rig2` 2026-09-21 (D2): `DEVPKEY_Device_LocationPaths` is the property, and the spelling held across a port power cycle and a sibling's absence. Still waiting on hardware for a move to a *different* port; reboot stability was proven on the
  same rig the same day.
- **C6 — the capture completion watermark**, waiting on one measurement
  against a live PHD2 (D9). Until it exists the facade cannot tell a
  finished exposure from the frame before it.
- **C5 — how device numbers stay stable**, waiting on a choice between
  binding by `UniqueID`, placeholder registrations, or upstreaming
  explicit numbering (D4.7). The pinned `ascom-alpaca-rs` assigns numbers
  by `Vec` position, so the sparse-slot approach this plan first proposed
  is not implementable against it.
- **C5 — how `config.apply` reports a restart-only field**, waiting on a
  choice between making all camera config restart-only and adding
  per-path dispositions to the shared config-actions API (D4.1).
  `ConfigurableDriver` has one disposition per driver today.
- **C4 — a tenet-3-safe CFW detection path**, waiting on a choice
  between deferring the probe to client connect, finding a non-actuating
  detection call, or an explicitly-unsafe operator command (D6). Blocks
  C4: the enumerate/probe split alone does not discharge the tenet.
- **C5 — QHY's serial-less identity policy**, waiting on a choice
  between giving those models a port-based identity (with the matching
  `devices`-key and shared-CFW-id migration) and documenting the existing
  `UniqueID` collision as known (D4.6).
- **C7 — its relationship to [`focus-model.md`](focus-model.md) S7/D17**,
  which retires rp's capture-based `auto_focus`. C7 must not land before
  that is settled, or the two plans race on one contract (D12).

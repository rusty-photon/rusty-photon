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

- **Part A — claims.** Each SDK-backed driver gains a `usb_devices` list
  that pins each USB port it serves to an Alpaca device number, so a
  camera on a given port is the same ASCOM device every time. The port
  path is **the only key**, resolved from a **passive** host USB scan and
  applied **before** any device-touching probe. A driver never opens a
  camera on an unlisted port. `doctor` lists every camera on the bus by
  port and prints a paste-ready list, so nobody types a port path from
  memory.
- **Part B — the PHD2 camera facade.** `phd2-guider` additionally serves an
  ASCOM Alpaca **Camera** device backed by PHD2's own frame capture, so the
  guide camera appears in rp's roster like any other camera and the ordinary
  `auto_focus` capture sweep works on the guiding train.

Part A makes Part B safe (the guide camera is deliberately left off the
vendor driver's list instead of accidentally skipped), and Part B makes
Part A worth doing (the unlisted camera is still reachable, through PHD2).

**But Part A does not make Part B safe *on its own*, and the plan must not
imply it does.** No list stays the permanent default — and with no list
the vendor driver serves every camera it can place, the guide camera
included — and turning on
`camera.enabled` neither knows nor validates which port the guide camera
occupies — so a rig can run the facade *and* let the vendor driver claim
the same physical camera, which is precisely the conflict this plan
exists to end. Two things close that, both in C6/C7 scope:

- the facade's own design doc stating a **vendor-driver `usb_devices`
  list that leaves the guide camera off as a hard prerequisite**, not a
  recommendation: enabling `camera.enabled` while the guide camera's
  vendor driver still serves it is a misconfiguration, and the facade's
  design doc says so;
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
| C0 | This plan | Merged; revised 2026-09-29 and 2026-10-01 | [#1263](https://github.com/rusty-photon/rusty-photon/pull/1263), [#1365](https://github.com/rusty-photon/rusty-photon/pull/1365) |
| C1 | **Hardware spike + passive USB identity**: confirm the Windows port spelling on the real box (direct and behind a hub, across replug and reboot), then implement `port` + `serial` extraction on all three collectors (new work on each — none extracts either today) and make inventory failure distinguishable from an empty bus | Landed: `port`/`serial` extraction, the failed-vs-empty inventory, the staged synthetic inventory, faults (a record that is not a working device is a fault, not a failed scan — D4.4), and the Linux port spelled by controller and USB revision instead of bus number (D2). The spike's last leg, a move to a different port, was made on a Windows VM rather than `rig2` (D2, spike item 7) | `chore/device-claims-c1-spike` ([#1306](https://github.com/rusty-photon/rusty-photon/pull/1306)), `chore/device-claims-c1-synthetic-inventory` ([#1308](https://github.com/rusty-photon/rusty-photon/pull/1308)), `fix/doctor-usb-faults-1322` ([#1365](https://github.com/rusty-photon/rusty-photon/pull/1365)), `feature/device-claims-linux-port-spelling` ([#1450](https://github.com/rusty-photon/rusty-photon/pull/1450)), `fix/device-claims-linux-fault-location` ([#1458](https://github.com/rusty-photon/rusty-photon/pull/1458)), `chore/device-claims-c1-port-move` ([#1462](https://github.com/rusty-photon/rusty-photon/pull/1462)) |
| C2 | `usb_devices` schema + `svbony-camera` — port placement (the join), listed numbers, placeholders (their fixed connect error code, which rp treats as permanent for the pass, and their config actions), the failed-scan re-scan, each simulation backend's synthetic inventory, and `svbony-camera doctor --devices` (D5's listing and paste-ready block; the `usb-devices.*` checks stay in C5), so no driver serves the list before its paste source exists. `usb_devices` is part of `config.schema`/`config.apply` from the phase that adds it, under the ordinary `Reload` disposition (D4.1), and D3's validation rejects a bad list in `config.apply` as well as at load. The easy case; proves schema, join and placeholder behaviour | Not started | |
| C3 | `usb_devices` in `zwo-camera`, with its `doctor --devices` listing; a failed identity open is a per-camera outcome (D7) | Not started | |
| C4 | `usb_devices` in `qhy-camera`, with its `doctor --devices` listing and each entry's declared filter wheel (D4.7) + `qhyccd-rs` enumerate/probe split — restores the documented enumeration-only contract, **and moves the CFW probe off startup and reload entirely** (the split alone narrows the tenet-3 problem, it does not discharge it) | Not started | |
| C5 | `usb-devices.resolve` / `usb-devices.unlisted` / `usb-devices.implicit` checks on top of the per-driver `--devices` listings, and central doctor's `service.devices` recognising placeholders (D5); **the breaking no-list changes for all three drivers at once** — numbering by port order, unplaceable cameras refused, and the port-based `UniqueID` fallback for serial-less cameras (ZWO/SVBony; QHY's case decided here, D4.6) | Not started | |
| C6 | `phd2-guider` Alpaca Camera facade on port 11128 (design doc → BDD → code): the completion watermark demonstrated against a live PHD2, the nested `camera` config block, `image_dir` + unit `ReadWritePaths=`, try-lock arbitration, and the catalog/packaging/firewall registration — plus the `save_image` wire-format fix | Not started | |
| C7 | rp wiring: guide camera as a train-terminal camera, capture-sweep focusing for the guiding train, doc updates. **Blocked on reconciling with [`focus-model.md`](focus-model.md) S7/D17**, which retires rp's capture-based `auto_focus` and keeps the metric sweep under that name | Not started | |
| C8 | `ui-htmx` editing of `usb_devices` | Deferred | |

Order: C1 first and **blocking** — no schema commits to a port spelling
that has not been proven stable on all three platforms. C2 → C3 → C4 then
land per driver, each on its own, adding the list path and leaving the
no-list path as it is today. C5 lands after all three: it flips the
no-list default in every driver at once (D4), so it inherits C4's
tenet-3 blocker and the macOS check in the waiting list. C6 needs
nothing from Part A but is only *operationally* safe after C4. C7 needs
C6.

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
`usb-devices.*` checks and `service.devices`' placeholder reporting.
Each driver's `doctor --devices` listing is documented in that driver's
design doc in the phase that ships it (C2–C4). `packaging.md` is updated
in C6 for the new port and the `image_dir` sharing steps.

---

## Part A — Device claims

### D1. The USB port path is the only key

Surveyed against the three vendored SDK crates and the host USB layer
rather than assumed:

| Candidate key | Exists for every camera? | Readable without opening the device? | Ambiguous? |
|---|---|---|---|
| **USB port path** | **Yes** — every device on the bus has one | **Yes**, on every platform | No — one device per port |
| SDK serial | No — many cameras report none | ZWO: no (needs `ASIOpenCamera`). QHY/SVBony: yes | No, when it exists |
| Model name | Yes | Yes | Yes — two cameras of one model share it, and on Windows the string is firmware-stamped, so a join reads VID:PID first (D2, spike item 5) |

Per-SDK detail for the serial route, for the record:

| SDK | Identity | Free at enumeration? |
|-----|----------|------------------------|
| QHYCCD | `GetQHYCCDId(index)` → `QHY268M-<serial>` | Yes — but the trailing field is a constant on models with no flash serial |
| ZWO ASI | `ASIGetSerialNumber`, else `ASIGetID` | No — needs `ASIOpenCamera`; older models (ASI1600) expose neither |
| SVBony | `CameraSN` in the enumeration struct | Yes |

**Decision: the USB port path is the device key, and the only one.** Serial
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
**`usb_devices` key**. The `devices` override map keeps its existing
serial-derived keys for cameras that have a real serial (D4.6 changes
only the serial-*less* fallback); a listed camera's overrides —
`name`/`description` and, in `qhy-camera`, its filter wheel's
`filter_names` — move into its `usb_devices` entry (D3), but C5 must not
read this decision as licence to re-key or remove valid overrides on the
no-list path.

The decisive property of the port key is that **USB identity is fully
passive on every platform**: Linux reads it from sysfs, macOS from
`system_profiler`, Windows from PnP properties — the kernel cached all of
it at enumeration, so nothing is opened, claimed, or reset. A serial-keyed
list could not have said the same: deciding which ZWO cameras to skip
would have required opening every one of them first.

### D2. The port path already exists in this repo

`rusty-photon-doctor-checks`'s `facts` module already enumerates USB
passively and cross-platform for the D4 `hardware.usb-device` check, with
**no third-party dependency**:

- **Linux** — walks `/sys/bus/usb/devices`, reading `idVendor`,
  `idProduct`, `product`. The **directory name is the port chain**
  (`1-4.2` = bus 1, root port 4, hub port 2), so C1 captures
  `entry.file_name()`. The `serial` attribute sits in the same directory,
  equally free. **Its leading bus number is not a topology fact,
  though.** The kernel hands out bus numbers in the order host
  controllers register, so on a host with more than one controller —
  the Pi 5, whose RP1 puts each USB 3 port on its own xHCI, or any PC
  with an add-in USB card — they can move after a kernel update or a
  change in probe order. Two cameras on different controllers would then
  swap device numbers silently, each still matching its own SDK camera,
  and no reboot test can rule that out. So before C2 commits the schema,
  the Linux key replaces the bus number with the controller it stands
  for: the controller's sysfs path (the PCI address or platform node
  above `usbN`), then the root hub's USB revision, then the port chain —
  the shape of udev's `ID_PATH_WITH_USB_REVISION`
  (`pci-0000:00:14.0-usbv3-0:4.2`), not plain `ID_PATH`, which gives a
  USB 2 and a USB 3 device on same-numbered ports of one xHCI the same
  name. It is built from sysfs alone (the entry's realpath and its root
  hub's `version`), so it stays passive. The `1-4.2` examples elsewhere
  in this plan stand for that spelling.

  **Landed, with one departure from udev for a device** (a root hub's
  own record, which udev gives no revision path, is spelled with no
  chain: `pci-0000:00:14.0-usbv3`). udev names the innermost
  platform device above `usbN`, and on mainline dwc3 boards (Rockchip,
  i.MX) that is the `xhci-hcd.N.auto` child the glue driver creates —
  an id the kernel allocates in probe order and marks with `.auto` for
  exactly that reason. Copying udev there would bring back the
  bus-number problem under another name, so the collector skips
  `.auto` names and names the nearest platform ancestor that has a
  stable one (`platform-fc000000.usb-usbv3-0:1`); a run of nothing but
  `.auto` names contributes nothing and the PCI device above it names
  the controller. A plain instance number is explicit and kept. The
  Raspberry Pi 5 is the case that mattered, and udev's spelling was
  already safe there: read on `pier1` 2026-10-08, its two RP1
  controllers are `xhci-hcd.0` and `xhci-hcd.1`, under
  `1f00200000.usb` and `1f00300000.usb`, and the Pi kernel takes that
  number from the devicetree's `usb` alias, not from probe order
  (`of_alias_get_id(…, "usb")` in its `dwc3/host.c`). So a Pi 5 port
  reads `platform-xhci-hcd.1-usbv2-0:2.1`, byte-identical to
  `udevadm info`. The collector itself was run against two real trees
  the same day and matched udev's `ID_PATH_WITH_USB_REVISION` for every
  device, with no faults: `pier1`'s 10, across both RP1 controllers, and
  19 on an x86 dev box, on two PCI xHCI controllers sitting one and
  three PCIe bridges deep. The `.auto` path was not checked on Rockchip
  hardware, because the Orange Pi 5 Ultra was unreachable. Its tests
  use a synthetic tree.

  A record whose spelling cannot be built is a fault (D4.4): a name that
  is no port chain, an entry under no root hub, a root-hub `version`
  that names no revision, an unreadable `subsystem` link, or a
  controller with no stable name. So is every record under two root hubs
  that end up with one spelling. Platform device names are unique, so
  that can only happen where one stable ancestor sits over two `.auto`
  controllers of the same revision. No observed host has one, but a
  collision would let one string name two sockets. A fault whose port
  could still be spelled (the collision, or a record whose `idProduct`
  could not be read) carries the spelling as its `location`, which is
  what D4.5's match of a fault to a listed port needs.
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
candidate device that reached the port placement (D4.2) without a port would be
indistinguishable from one whose port simply did not match a listed port — a
silent mis-resolution in the one field ownership depends on. So a
candidate record without a port never enters the inventory: it is
reported as a **fault** (D4.4) and nothing selects or opens it; `Option`
survives only as a compatibility shim for older serialized fixtures,
which the runtime path rejects.

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
   element is: a record with no usable port. C1 first classified that as
   an inventory failure, and on `rig2` it proved far too coarse — the
   descriptor-failed record is a permanent placeholder on a root-hub port
   (`USB\VID_0000&PID_0002`, problem code 43, surviving reboots), and it
   blanked every presence answer on a host where every real device was
   working. It is now a **fault** under D4.4: reported, left out of the
   inventory, and no longer able to cost the answer for anything else.
   This shape was not anticipated — the plan described the property as
   though it held one string.

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
   throughout. A genuine move to a *different* port needed physical
   access to `rig2`; it was made on a Windows VM instead (item 7). With
   the cold re-enumeration the one exception, the key is proven on
   Windows across every transition C1 set out to test.

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

   **Correction (2026-09-28, re-read on `rig2`):** `QHY5IIISeries_IO` is
   the Windows *friendly name* both cameras get from QHY's INF, not what
   they publish on the bus. `DEVPKEY_Device_BusReportedDeviceDesc` — the
   property the collector reads — is `QHY600U3G20-20230614` and
   `QHY678U3G20-20230106`, distinct and firmware-stamped. The model
   strings therefore do not collide; reading VID:PID first is still the
   right order (the suffix reads as a firmware date, which a firmware
   update would change where the product id would not), but not because
   the names are equal.

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
   indistinguishable once enumerated. Placement treats each half as its
   own port and reports a camera found on the twin of a listed port as a
   speed fallback (D4.8); the power-cycle evidence in (3) showed the
   spelling stable when the speed did not change. `rig2`'s ASI662MC, a
   USB 3 camera on the `USB2807` half, is either in a USB 2.0 connector
   or already fallen back to USB 2 speed. Which of this rig's
   connectors are the USB 2.0 pair is likewise unknown — the vendor does
   not document the panel layout, so it needs a look at the hardware.

7. **A move to a different port follows the socket — measured on a VM,
   2026-10-10, not on `rig2`.** The move needed someone at `rig2`, a
   remote site, but not `rig2`'s hardware: the location path is built by
   Windows' own USB stack, which runs unchanged against a virtual xHCI.
   So the leg was run on the dev box's Windows 11 25H2 KVM guest
   (QEMU 10.2.2, a `qemu-xhci` controller with 15 ports), with the host's
   ASI1600MM-Cool (`03c3:1603`, no USB serial) passed through at a chosen
   guest port (libvirt's `<address type='usb' bus='0' port='N'/>`).
   Detaching it and attaching it at another `N` is a move with no cable.
   Each step was read by the real collector, built in the guest from
   `main` at `1c367361`, and by `Get-PnpDevice` without `-PresentOnly`:

   | Step | The camera's `port` in the collector's inventory | PnP records for `03c3:1603` |
   |---|---|---|
   | Attach at port 4 | `PCIROOT(0)#PCI(0201)#PCI(0000)#USBROOT(0)#USB(4)` | `…\6&1C4D2F9B&0&4` present |
   | Move to port 9, which this camera had never used | `…#USB(9)` | new `…&0&9` present; `…&0&4` not present |
   | Move to port 12 | `…#USB(12)` | new `…&0&12` present; `…&0&9` not present |
   | Back to port 4 | `…#USB(4)`, the whole inventory byte-identical to the first read | `…&0&4` present again |
   | Detach | not listed | none present |

   No read had a fault or a failed scan, and the camera was never on two
   ports at once. (The guest's controller numbers its SuperSpeed ports
   first: the USB 3 camera at guest port `N` read `USB(N)`, and the USB 2
   tablet on guest port 1 read `USB(16)`.)

   **What the move taught: Windows keeps every port a device has used,
   with its old path.** A device with no USB serial gets an instance id
   built from its port — `…&0&4` on port 4, the same form as `rig2`'s
   `6&4213695&0&1` and `6&4213695&0&3` — so a move creates a new record,
   and the old one stays behind, marked not present
   (`CM_PROB_PHANTOM`) and **still carrying its old `LocationPaths`**.
   The guest held nine such records for ZWO cameras from earlier
   sessions before this one began. A collector that read every record
   would put a camera on every port it had ever used. This one reads
   `Get-PnpDevice -PresentOnly`, and no stale record reached its
   inventory at any step.

   What it does not cover: a hub between the controller and the camera
   (not tried; the hub hop's spelling and its stability were read on
   `rig2`, items 1 and 3); a real vendor controller; and a device that
   publishes a USB serial, for which Windows keeps one record that moves
   with the device. That last case is untested, but no camera seen so
   far publishes a serial (item 4).

**The port string is the platform's native spelling, not a normalised
invention.** udev's `ID_PATH_WITH_USB_REVISION` on Linux (with the one
departure above), the location path on Windows, the location id on
macOS. A config file already names one specific host's hardware; it
is not portable across an OS boundary, and inventing a canonical form only
creates a second thing that can disagree with what the OS says. Doctor
prints the exact string to paste (D5), so the operator never types one.

### D3. Config shape

**Revised 2026-09-29.** An earlier draft offered three `claims` modes —
`all`, `include` (a port allow-list) and `exclude` (a port deny-list).
They are replaced by **one explicit list that pins each USB port to an
Alpaca device number**. The purpose is narrower and more useful than
"ownership": *a camera plugged into a given port shows up as the same
ASCOM device every single time*. Adding a camera, or moving one to a
different port, is a config edit — an observatory seats a cable once,
and that cost is accepted (D1). Ownership falls out of the same list: a
camera on an unlisted port is never opened.

Added to each camera service's `Config`:

```json
"usb_devices": [
  { "device_number": 0,
    "usb_port": "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(14)#USB(1)",
    "name": "QHY600M",
    "filter_wheel_number": 0,
    "filter_names": ["L", "R", "G", "B", "H", "S", "O"] }
]
```

That is `rig2`'s `qhy-camera` list. Its QHY5III678M on
`…#USB(14)#USB(3)` is the guide camera PHD2 owns, so it is deliberately
not listed (see below).

- **`device_number`** — the Alpaca device number this port's camera is
  served under. Explicit rather than positional, so reordering entries
  renumbers nothing, and no number ever moves unless the operator writes
  the change. Retiring any camera but the highest-numbered one means
  renumbering the entries above it, and rp's `cameras[].device_number`
  with them, because a gap is rejected (below). A retired camera's entry
  is never kept just to hold its number: a listed port is a positive
  claim, so whatever camera of this SDK is plugged into that socket
  later would be opened and served at that number — PHD2's guide camera
  included.
- **`usb_port`** — the port path in the platform's native spelling (D2),
  pasted from `doctor --devices` (D5), never typed from memory.
- **`name`**, **`description`** — optional display overrides.
- **`filter_wheel_number`**, **`filter_names`** (`qhy-camera` only) —
  the CFW that shares this camera's handle, declared rather than
  discovered: its Alpaca FilterWheel number, pinned exactly as
  `device_number` pins the camera's, and its slot names, replacing the
  generated `Filter0..N` (D4.7). A listed camera with no
  `filter_wheel_number` registers no wheel.

Every override a listed camera's `devices` entry could carry lives in
its `usb_devices` entry, the one place that already names it. The
serial-keyed `devices` override map stays only for the no-list default
below, and **a non-empty `devices` map next to a list is rejected at
load**, naming each key and telling the operator to move its fields into
the matching entry, or to delete it if that camera is unlisted. An
override that would be silently ignored is the no-op config the tree's
fail-loud posture exists to prevent.

Validated at load, before any SDK work, with the offending entry named:

- `usb_port` non-blank, free of leading or trailing whitespace, and
  unique across entries. A padded port is rejected rather than trimmed:
  it is compared verbatim against the USB inventory, which never carries
  padding ([doctor.md](../services/doctor.md), "USB inventory"), so it
  could only ever become a placeholder for a port that holds a camera.
- `device_number` unique, and the set exactly `0..N-1`.
- `filter_wheel_number` unique across the entries that set one, and that
  set exactly `0..M-1`; `filter_names` only beside a
  `filter_wheel_number`, with the validation `devices.<id>.filter_names`
  gets today.
- `deny_unknown_fields`, as everywhere else in the config tree.

A gap is rejected rather than filled with a placeholder, so every number
a driver serves is one the operator wrote. The served numbers must be
dense: the pinned server numbers each device type by registration
position and rp binds a camera by its position among the server's
cameras (D4.7). Making the operator write every one keeps a typo
(`0, 2`) from silently becoming a phantom camera 1. The [Alpaca API
reference](https://github.com/ASCOMInitiative/ASCOMRemote/blob/619946880f5be17b39efda4fb116b925300846f6/Documentation/ASCOM%20Alpaca%20API%20Reference.pdf)
§2.1.3 itself requires only that each type's numbers start at 0 and be
unique; density is what the fork and rp need.

The same validation runs in `config.apply` (`ConfigurableDriver::validate`)
from the phase that adds the field (C2–C4), and a changed list applies
through the ordinary reload (D4.1).

`"usb_devices": []` registers nothing — legal when written by hand, and
logged loudly. It is how an operator hands a whole SDK to another
application for a night, and it opens nothing, so it needs no USB scan.
A server that registers no device has nowhere for the config actions to
live — they hang off each registered device, and `ui-htmx` reaches a
driver only through an rp roster entry's device number — so `[]` is left
by editing the file and reloading (`SIGHUP`, the SCM `ParamChange`
control, or a restart), never over Alpaca. `config.apply` therefore
refuses an empty list and says why: the apply would remove the path it
arrived on, the same self-lockout `ui-htmx` already guards against for a
device's `enabled` flag.

**No list at all is the default, permanently** — no deprecation, no
release that demands the list. With no list the driver registers **every
camera the USB scan can place, numbered by port order** (D4.7), and
refuses any camera it cannot place (D4.3, D4.4). A single-camera rig
never needs the list. Schema compatibility is not behavioural
compatibility, though: C5 changes the no-list numbering and the identity
of serial-less cameras, and starts refusing cameras it cannot place, so
an existing file stays valid while the bindings it feeds — rp's
`cameras[].device_number`, `devices` overrides keyed on a `noserial-*`
identity — may still need revisiting (see the breaking-change note under
D4). The nudge toward an explicit list is doctor's `usb-devices.implicit`
finding (D5), whose paste-ready block is offered only when a driver
without a list registered more than one camera; the same check reports
the cameras the no-list default refused, whatever the count.

**The guide camera PHD2 owns is protected by leaving it off the list.**
A listed port is a positive statement, so the failure modes are the safe
ones: a camera moved to an unlisted port is simply not opened, and the
number it used to answer to becomes a placeholder that says no working
camera is on that port (D4.5) — visible, not damaging. There is no
deny-list form, by design: a deny-list is negative, so a moved guide
camera would land on an
unlisted-and-therefore-allowed port and get opened, recreating the exact
conflict Part A exists to end.

A comment naming the camera is the operator's business; the config does
not carry a second identity for the same device that could fall out of
date. (`name` is display text, never a key.)

### D4. Behaviour

1. **The list is applied before any device-touching call.** The one rule
   that cannot be relaxed per driver. The USB inventory yields ports
   passively; each SDK camera is placed on a port (D4.2); only cameras
   on listed ports — or, with no list, placeable cameras (from C5; until
   then the no-list path opens every SDK camera, as today) — are opened,
   probed or initialised. A camera on an unlisted port is never opened,
   not even to read its serial.

   **A changed list applies through the ordinary reload**, which rebuilds
   every device on the service: each open camera is closed, its clients
   must reconnect, and an exposure in flight is lost — including on
   cameras whose entries did not change. An operator who edits the list
   is re-cabling or re-assigning cameras, not in the middle of an imaging
   session, so that is acceptable on `config.apply`, and no restart-only
   disposition or per-path apply machinery is needed. (An earlier draft
   required restart-only on the premise that the change could land
   mid-exposure; that premise was rejected.) What a reload must still
   never do is **actuate**: [workspace tenet 3](../workspace.md#project-tenets)
   binds every reload, with or without this list, and `qhy-camera`'s
   reload reruns the CFW probe, whose precondition `InitQHYCCD` may
   auto-home a filter wheel — the vendor's statement, measured not to on a
   QHY178M + CFW3 or a QHY600M
   ([`qhy-camera.md`](../services/qhy-camera.md) C5). That is C4's problem
   for every reload (D6), not a property of this field.
2. **Placing an SDK camera on a port is a join.** None of the three SDKs
   reports a USB location: `ASI_CAMERA_INFO`, `SVB_CAMERA_INFO` and
   `GetQHYCCDId` give an index, a name and — for QHY and SVBony — a
   serial, never a port. The USB scan knows
   `(port, vid, pid, product string, serial?)`; the SDK knows
   `(index, model, id/serial?)`.

   **The candidates** on the bus side are the working inventory records
   this driver's SDK would enumerate as cameras: its vendor id, and a
   VID:PID its normalizer (below) maps to a camera model. A record that
   shares the vendor id but is not a camera is never a candidate — the
   ZWO EAF and EFW sit on `03c3` like every ASI camera — so it is never a
   `zwo-camera` row, join or unlisted camera. Where the SDK offers a
   passive check, it settles the question for a VID:PID the normalizer
   does not know (ZWO's `ASICameraCheck(vid, pid)`).

   They are matched on the strongest signal **both** sides carry
   passively: serial when the bus record and the SDK camera each have
   one, else VID:PID, then the product string. A match places a camera
   only when it is **one-to-one**: the SDK camera's key matches exactly
   one candidate record, *and* that record's key matches exactly one SDK
   camera. Uniqueness on the bus side alone is not enough — two SDK
   cameras of one model beside a single working record (the other's
   record a fault, or its port unreadable) would otherwise put either
   camera on the one port.

   In practice the serial route is rarely open: on `rig2` (Windows) and on
   the Linux dev box, **no camera publishes a USB serial** — not the
   QHY600M, QHY5III678M or ASI662MC, not the QHY178M or ASI1600MM — so
   the bus side has none to match. **Serial and model are join signals
   here, never keys** — the operator never writes them. Both comparisons
   need per-SDK canonicalization: QHY's id is `QHY268M-<serial>`, not a
   bare serial, and the SDK's model name differs from the bus-reported
   product string (which on Windows is firmware-stamped, D2 spike item
   5). Each driver supplies its own normalizer, with unit tests over real
   observed pairs — an un-normalized compare finds no camera for a record
   while staring at the right one.

   **A model the normalizer does not know is paired by elimination.** A
   normalizer only knows the models someone has observed. After the keyed
   matches, the one SDK camera and the one record left unpaired are
   paired when nothing else could explain either: exactly one SDK camera
   and exactly one record with this driver's vendor id remain unpaired
   (records the normalizer or the SDK's passive check identifies as
   non-cameras excluded), no fault carries this driver's vendor id, and
   the record's VID:PID is not one the normalizer assigns to a different
   model. Both sides are then complete for this SDK, so this is a
   deduction, not a guess — and it is what keeps a single-camera rig
   whose model nobody has observed yet working after C5. A camera still
   unpaired after elimination is **unrecognised** (D4.3).
3. **An unresolvable join is reported, never guessed.** Registering the
   wrong camera is worse than registering none: a guess means the guide
   camera answers to the main camera's device number, and the first
   symptom is a subframe from the wrong sensor at 2am.

   **Two things cannot be resolved: look-alikes and unrecognised
   models.** A **look-alike** is an SDK camera the join cannot pair
   one-to-one (D4.2) on signals both sides carry passively. The usual
   case is two or more cameras of one SDK, on one host, identical on the
   bus — same VID:PID and product string, no USB serial — which leave
   nothing to tell which SDK index sits on which port. It also covers:

   - **Any two identical ZWO cameras**, whatever their USB descriptors
     carry: ZWO's serial is readable only after `ASIOpenCamera` (D1),
     which placement must not call (D4.1), so the SDK side has no serial
     to match. Likewise a QHY model whose `GetQHYCCDId` serial field is a
     constant (D1).
   - **Two identical cameras the SDK enumerates while only one has a
     working record** — nothing says which of them the missing record
     belongs to, so neither is placed.

   Cameras of different sensor families do not hit it: the reference
   rig's QHY600M (`1618:c601`) and QHY5III678M (`1618:0679`) differ on
   the product id alone. Variants *within* one QHY family very likely
   do. QHY loads one firmware image per family, not per mono/colour
   variant (`85-qhyccd.rules` maps loader PID `c178` to `QHY178.img` and
   `c268` to `QHY268.img`); the dev box's QHY178M enumerates as
   `1618:c179` / `Q178-Cool` with no mono/colour marker, and `rig2`'s
   QHY600M as `QHY600U3G20-20230614`. So a QHY268M and a QHY268C on one
   host are treated as look-alikes unless C4 captures such a pair and
   shows the bus tells them apart.

   An **unrecognised** camera is one elimination (D4.2) could not pair
   either: its model is unknown to the normalizer and more than one
   camera or record is left over.

   Both are **refused**, whatever the configuration:

   - **Listed:** the number is served by a placeholder (D4.5) whose
     `Connect` fails naming the collision or the unknown model, and
     doctor reports it (`usb-devices.resolve`, `warn`).
   - **No list:** not registered at all; every other camera is. The
     driver logs it and doctor names it (`usb-devices.implicit`, `warn`).

   Doctor reports an unrecognised camera's SDK model and the VID:PID the
   bus shows — everything a normalizer entry needs. The fix for
   look-alikes is cameras of different sensor families, or the
   look-alikes on separate hosts; for an unrecognised model, a normalizer
   entry.

   **One way to resolve look-alikes exists, and it is deliberately not
   built yet.** When *every* look-alike is the driver's own (all listed),
   the driver may open each in turn and ask the OS which USB device the
   process just opened — on Linux, the usbfs file the SDK holds open
   (`/dev/bus/usb/BBB/DDD`, whose bus and device number lead to the
   sysfs entry and so to the port); on Windows, the open handle's device
   path, whose instance id leads to the location path. It needs
   platform-specific code that depends on how each SDK holds its USB
   handle, and it can never help when a look-alike belongs to another
   application, because opening that camera is exactly what this design
   forbids. Recorded so the option is not lost; built only when a rig
   actually needs two identical cameras on one host.
4. **The USB inventory decides what may be opened, so its failures are
   explicit.** Every collector used to return an empty `Vec` when its
   source failed, which read as "no devices" — indistinguishable from an
   idle bus, and an invitation to a fallback that opens every SDK camera
   and defeats the ownership boundary. The scan now has three outcomes,
   and every consumer must keep them apart:

   - **The inventory** — devices that are alive and working, each with
     its port.
   - **Faults** — records on the bus that are not working devices: the
     platform reports them not working (a Windows problem code,
     including the placeholder Windows leaves for a device whose
     enumeration failed), or their identity or port could not be read.
     Reported, left out of the inventory, never a failed scan.
   - **A failed scan** — the collector itself could not run: the source
     unreadable, the shell-out erroring or timing out, its output
     unparsable — output the query cannot have produced, such as a
     Windows line that is not a five-field `USB\VID_` record (the query
     replaces control characters in device-supplied text, so a device's
     own strings cannot produce one), or a macOS report with no
     `SPUSBDataType` list. That is the collector's failure, not a
     device's, and reading it as a thinner bus would hide it.

   **Not every entry is a candidate device record.** The collectors
   legitimately skip a great deal: the Linux walk passes over interface
   entries that have no `idVendor` at all, the Windows query filters out
   root hubs and other non-`USB\VID_` instances and the parser skips a
   composite device's per-interface children (`…&MI_nn`), and the macOS
   tree contains
   non-device nodes. Those are skipped silently. A *candidate* that is
   not a working device is a fault. C1 first failed the whole scan over
   one such record, and on `rig2` a single permanent Windows placeholder
   then blanked every presence answer on a host where every real device
   worked (D2, spike item 2). Only the collector itself failing fails
   the scan; `Ok(empty)` still means a genuinely empty bus.

   **The inventory is complete over working devices, not over everything
   on the bus, so nothing may be concluded from a device's absence.** A
   camera is opened only on a positive match between a working
   inventory record and what the SDK enumerates. A camera the SDK sees
   with no one-to-one match to a working record (D4.2) — its record is a
   fault, it sits on no port the scan could read, it cannot be told
   apart from one that does, or nothing joins it — is never opened.

   **What each outcome means for a driver:**

   | | List configured | No list |
   |---|---|---|
   | A listed / placeable camera in the inventory | registered at its number | registered, numbered by port order |
   | A listed port with no working camera found on it — nothing enumerated there, a fault located there, or a working device no SDK camera is placed on | placeholder at its number (D4.5) | — (nothing to place) |
   | A camera on an unlisted port | never opened | — |
   | Look-alikes and unrecognised models (D4.3) | placeholders at their numbers | not registered |
   | A placed camera the driver fails to open — ZWO's identity read (D7), QHY's stage 2 while it still runs at startup (D6) — because another process holds it or it is wedged | placeholder at its number, naming the SDK error | not registered; logged at `warn!` |
   | **The scan failed** | **placeholders at every listed number; nothing opened** | **nothing registered** |

   With no list, a refused camera shifts the port-order numbers of the
   cameras after it (D4.7); a list is what stops that.

   A failed scan therefore leaves a no-list rig with **no cameras**, and
   that is deliberate: a camera whose port cannot be read cannot be
   numbered by it, and registering it anyway would hand out device
   numbers that change with the next scan. The driver logs the scan
   error at `error!`, and doctor's USB checks fail and name the host
   fault: central doctor's `hardware.usb-device` and the per-service
   `usb-devices.resolve` / `usb-devices.implicit` (D5). Nothing else in
   this plan makes doctor fail over USB; a device absent from the bus
   keeps central doctor's existing severity (`fail` for an installed,
   enabled unit, [doctor.md](../services/doctor.md) "Hardware").

   **A failed scan is not final.** The collector's 10 s deadline is sized
   to tell a wedged child from a working one, but a healthy host under
   load can also run it out — most likely a Windows box at boot, with
   the whole fleet starting at once (doctor.md measured a loopback probe
   at ~0.3 s idle and ~4.8 s under contention). A failed scan is
   deliberately never a startup failure (D4.5), so the service manager's
   restart-on-failure never retries it, and left alone one slow
   `powershell.exe` at boot would cost the whole night. So a driver whose
   scan failed serves the failed-scan outcome at once, without delaying
   startup, then re-scans in the background — after 10 s, 20 s and
   40 s, then every 60 s — and the first successful scan fires the
   driver's own reload, which re-runs placement exactly as `config.apply`
   does (D4.1). That is safe because the failed-scan outcome has opened
   nothing, so there is no session to interrupt, and because the reload
   runs the startup path, which tenet 3 binds exactly as it binds startup
   (C4). It is not hot-plug (D8): the re-scan runs only while the scan is
   failing and stops at the first success; a listed port that is merely
   empty stays a placeholder until the next reload. The scan error is
   logged at `error!` once and at `debug!` on each repeat, and the
   recovery at `info!`. A staged `--usb-inventory` failure fails the same
   way on every re-scan, so the failed-scan scenarios stay deterministic.

   **This makes the USB scan load-bearing for every camera, on every
   platform, and that has consequences to watch:**

   - **Simulation builds.** `qhy-camera`, `zwo-camera` and
     `svbony-camera` built with their `simulation` feature fabricate one
     camera each (`QHY178M-Simulated`, `ASI2600MM-Pro-Simulated`,
     `SV605CC-Simulated`) that no host scan can see; under the no-list
     default they would now register nothing,
     and the BDD and ConformU binaries depend on them. So each
     simulation backend **ships a synthetic inventory for its own
     fabricated cameras** — one working record per camera, on a
     synthetic port — through C1's staged-inventory mechanism, and the
     hidden `--usb-inventory <file>` flag replaces it when a scenario
     needs a different topology (a listed port empty, a fault,
     look-alikes, a failed scan). The tests then exercise the real
     placement rather than a special case around it.
   - **Hosts whose scan cannot place devices register no cameras.** That
     includes Windows on ARM and USB-over-IP clients (their devices
     publish no `PCIROOT(` path, so they are faults — deferred, outside
     the supported deployment), and macOS if `system_profiler` stops
     providing `SPUSBDataType` (reported for macOS 26, unverified). A
     report without that list is a failed scan, not an empty bus, so the
     cause would at least be named — but it would still mean "no
     cameras on macOS", so it is checked before C5 flips the no-list
     default (see the waiting list) rather than discovered in the field.

   **A collector that never returns is a fourth state.** The macOS and
   Windows collectors shell out; C1 bounds both invocations with a
   deadline, kills the child on expiry, and maps that to a failed scan,
   so a wedged `system_profiler` or `powershell.exe` cannot hang startup
   or reload.

   **The shared boundary carries all three outcomes.** `facts::gather`
   returns `HardwareFacts` with `usb`, `usb_faults` and
   `usb_unavailable` side by side, and its startup and doctor consumers
   must read the failure marker; a consumer that ignores it gets no
   devices rather than a plausible-looking partial list.

   **C1 built the mechanism; C2–C4 use it.** A JSON document replaces the
   collector's result wholesale under the shared crate's `mock` feature —
   it is the three inventory fields of `HardwareFacts` (devices, faults,
   failure) under their own names, so a `hardware` object captured from a
   real rig stages unchanged instead of being invented. It cannot express
   a state a collector could not produce: a failure paired with devices or
   faults, or a device with no port, is rejected at the boundary rather
   than reaching a consumer as a plausible-looking bus — a record without
   a port is staged as a fault, exactly as a collector reports it. Each
   driver exposes it as a hidden `--usb-inventory <file>` flag under its
   own `simulation` feature — the same affordance shape as doctor's
   `--platform-facts`, and absent from release builds for the same
   reason. The contract is in [doctor.md](../services/doctor.md) under
   "USB inventory".
5. **A listed number whose camera cannot be served gets a placeholder.**
   A listed port with no working camera found on it, a listed look-alike
   or unrecognised model, a listed camera the driver failed to open, or
   every listed port when the scan failed: the driver registers a
   **placeholder** Camera device at that number, so the other cameras
   keep theirs. A powered-down hub is a normal Tuesday; a driver that
   renumbered — or refused to start — because one of three cameras is
   absent would be worse than one that serves two and says plainly where
   the third went.

   - `Connected` reads `false`, and `PUT connected=true` fails with one
     fixed driver-specific ASCOM error code — the same value in all three
     drivers, from the `0x500`–`0xFFF` driver range — and a message
     naming the port and the reason **as of the last start or reload**
     (a placeholder never re-scans). The reason is one of:
     - the reason of a fault whose `location` is the listed port in its
       native spelling. A record Windows enumerated but reports not
       working (no driver, code 28) keeps its `PCIROOT(` path, so it
       matches; so does a Linux sysfs record whose identity could not be
       read;
     - the device that is there, when the port holds a working record no
       SDK camera is placed on — another vendor's device, a non-camera, a
       QHY camera whose firmware never loaded, one the SDK cannot see:
       *"PCIROOT(…)#USB(14)#USB(3) holds `<product>` (`<vid:pid>`), which
       the QHY SDK does not report as a camera"*;
     - the camera on the port's USB 2.0 twin (D4.8);
     - the look-alike collision, the unrecognised model, the SDK's open
       error, or the scan error;
     - otherwise *"no working camera is enumerated on
       PCIROOT(…)#USB(14)#USB(3)"*. It never says the port is empty,
       because absence proves nothing (D4.4): on Linux a device that
       fails enumeration leaves no sysfs entry at all, and Windows'
       enumeration-failure placeholder publishes only an `ACPI(…)`
       chain, which matches no listed port (`rig2`'s code-43 record, D2
       spike item 2). When the scan reported faults it could not place
       on a port, the message adds that and points at doctor's
       `hardware.usb-fault`.

     Every message ends with the way back: once the camera is fixed,
     reload or restart the service, which re-opens every camera it serves
     (D8). The fixed code lets a client tell "no camera here until the
     next reload" from a transient failure without parsing the message,
     and rp uses it (D4.7). Every other member behaves as for any camera
     that is not connected.
   - A Platform 7 `Connect()` cannot carry that error. Every camera
     reports `InterfaceVersion` 4, and the pinned `ascom-alpaca-rs` fork
     answers `PUT /connect` at once, runs `set_connected` in a task of
     its own, and sends a failure only to its own `error!` log — for
     every camera, not only a placeholder. The client sees `Connecting`
     return to `false` and `Connected` stay `false`, with no message. So
     the reason also lives where a client can read it without
     connecting:
   - Its name is the entry's `name` when given, else one naming the
     port. Its `Description` says it is a placeholder **and carries the
     same reason**, readable by a client, the Chooser or an operator.
   - Its `UniqueID` has one fixed, recognisable form,
     `placeholder:<service>:<usb_port>`. No real camera's can take that
     form, so a client that remembers devices by `UniqueID` (NINA does)
     does not mistake the stand-in for the camera, and central doctor
     can recognise it in `configureddevices` (D5).
   - It carries the service's config actions exactly as a registered
     camera does: `SupportedActions` lists `config.get` /
     `config.schema` / `config.apply`, and `Action` serves them whether
     or not it is connected. An rp camera entry bound to that number is
     how `ui-htmx` reaches the service, and a missing or moved camera is
     exactly when the operator needs that page to fix the list.
   - The driver logs `warn!` naming the port; doctor reports a *soft*
     finding (`usb-devices.resolve`, `warn`), and fails only when the
     scan failed (D5). Never a startup failure.
   - A placeholder only ever stands in for a **listed** number: with no
     list, nothing declares a slot, so an absent camera simply is not
     registered. In `qhy-camera`, a placeholder camera's declared filter
     wheel is a placeholder FilterWheel on the same terms (D4.7).

   A client sees a camera that cannot connect tonight, which is exactly
   what is true. The cost is that ConformU, pointed at a placeholder,
   fails its connect test — acceptable, since the conformance runs use
   the simulation builds with no list.
6. **The port gives serial-less ZWO and SVBony cameras a stable
   `UniqueID`.**
   `zwo-camera`'s `mint_identity` falls back to `noserial-{index}` when a
   camera exposes neither serial nor flash id, and its own doc comment
   names the weakness: *"two serial-less cameras of the same model
   reordered on the bus … could swap identities."* Under this design
   every camera a driver registers has a known port — listed, or placed
   by the scan — so that fallback becomes `noserial-{port}`, stable
   across bus reordering and replug, **in `zwo-camera` and
   `svbony-camera`, the two drivers that mint such a fallback**. QHY
   mints none (its raw `GetQHYCCDId` string is the identity), so its
   equivalent is a C5 decision, not this change; see the QHY note after
   D4.8.

   This also removes the problem an earlier draft left open: it wanted
   the port-derived identity *persisted* after its first successful
   determination, because a camera could then be registered without a
   port (under `mode: "all"` with a failed scan) and would otherwise
   publish a different `UniqueID`. No such camera exists any more — a
   camera whose port cannot be read is not registered (D4.4) — so the
   identity is recomputed from the port on every start and needs no
   store, and `materialize_identity` (fixed JSON pointers, deliberately
   unused by the camera services) is not involved.

   It **changes `UniqueID` for affected cameras**, so it lands with C5
   and its documentation, not silently inside a driver phase (see the
   breaking-change note below). For a serial-less camera the identity
   now follows the socket: move it to another port and it becomes a
   different device — consistent with the port being the key, and
   exactly what the list already asks the operator to edit.
7. **Device numbers are stable by construction.** With a list, a camera
   answers to its entry's `device_number`, always; an entry whose camera
   cannot be served holds its number with a placeholder (D4.5), so no
   other camera moves. With no list, cameras are numbered in **port
   order**: ports are compared hop by hop from the root, each hop as a
   number in the base the platform writes it in. On Linux that is the
   controller, then the decimal port chain, so `1-4.2` precedes
   `1-4.10`. On Windows the `LocationPaths` segments are compared in
   order, `USB(n)` as decimal and `PCI(xxxx)` as hex. On macOS the whole
   `location_id` is one 32-bit integer; it is fixed-width lowercase hex,
   so that matches plain string order, where digit-run "natural"
   ordering would put `0x14a00000` before `0x14200000`. The order is
   stable for as long as the set of connected cameras does not change
   and each negotiates the same USB speed (D4.8); removing one still
   shifts the cameras after it, and a multi-camera rig that wants more
   uses the list.

   An earlier draft left this open between three options — binding by
   `UniqueID` in rp, placeholder registrations, or upstreaming explicit
   numbering into `ascom-alpaca-rs` — because the pinned server assigns
   numbers by `Vec` position (`iter_all()` enumerates each kind's
   registration list; there is no way to register at a chosen number).
   **Placeholders are the choice.** Numbers stay dense from 0. Starting
   at 0 is what the
   [Alpaca API reference](https://github.com/ASCOMInitiative/ASCOMRemote/blob/619946880f5be17b39efda4fb116b925300846f6/Documentation/ASCOM%20Alpaca%20API%20Reference.pdf)
   §2.1.3 requires ("starting at 0 for each device type"); beyond that
   it asks only that a number be unique within its type. Density is what
   the fork and rp need: the fork needs no change, and rp's binding
   needs none either, because rp binds `cameras[].device_number` to the
   camera's *position* among the server's cameras in
   `configureddevices` (`establish_camera` in
   `services/rp/src/equipment/camera.rs`), which equals the number
   exactly when the numbers are dense. Sparse numbering would have
   needed a fork change *and* an rp change to bind by the real number,
   and would still break the start-at-0 rule whenever device 0 was the
   absent one. ConformU, the ASCOM Chooser and NINA all take
   `DeviceNumber` verbatim from `configureddevices`, so none of them
   cares either way; the Chooser re-identifies a device by
   `UniqueID` + host + port + number, which is one more reason for
   numbers that never move.

   **rp does need one small change elsewhere.** `establish_camera` maps
   every `set_connected` error to a transient outcome, so left alone it
   would retry a placeholder three times with 1 s + 2 s backoff and log
   two `info!` lines on every reconnect-supervisor pass, all night —
   retries that cannot succeed, because a placeholder becomes a camera
   only on a reload (D8). rp therefore maps the placeholder's fixed error
   code (D4.5) to the permanent outcome its "camera not found" already
   gets: one attempt per pass, logged at `debug!`, and the camera is
   picked up on the first pass after a reload serves it.

   **`qhy-camera`'s filter wheels are pinned the same way.**
   `qhy-camera` registers a FilterWheel for each camera with a CFW, and
   rp binds `filter_wheels[].device_number` by position among the
   server's FilterWheels (`establish_filter_wheel` in
   `services/rp/src/equipment/filter_wheel.rs`), taking filter names
   from its own config, never from the device. Numbered in camera order,
   a placeholder camera with a CFW would shift every wheel after it, and
   rp would connect to another camera's wheel without error and move its
   filters — the wrong-device outcome D4.3 rules out. So a `qhy-camera`
   entry **declares** its camera's wheel (D3): `filter_wheel_number`
   pins the wheel's number exactly as `device_number` pins the camera's,
   and `filter_names` names its slots. A declared wheel is registered at
   its number whatever happens to its camera; when the camera is a
   placeholder, the wheel is a placeholder FilterWheel with the same
   reason. Because the wheel is declared rather than discovered,
   registering it needs no startup `InitQHYCCD` probe — whether a CFW
   actually answers is learned on the wheel's own `Connect`, which is
   C4's option 1 (D6) for every listed camera. A CFW that is plugged in
   but not declared is not registered. The no-list path keeps
   discovering wheels, in whichever tenet-3-safe way C4 settles.
8. **A camera that falls back to USB 2 speed is on a different port.**
   A USB 3 connector is wired to two hubs: the SuperSpeed one and its
   USB 2.0 companion (`rig2`'s UPBv2 is the `USB5807` / `USB2807` pair,
   D2 spike item 6). A USB 3 camera whose SuperSpeed link does not
   train — a marginal or USB 2 cable, a loose seat — enumerates on the
   companion under a different native spelling: `…-usbv2-0:1.3` (the
   USB 2 root hub) instead of `…-usbv3-0:1.3` on Linux,
   `…#USB(2)#USB(3)` instead of `…#USB(14)#USB(3)` on Windows. The chain
   need not stay the same — the two root hubs number their ports
   independently, and only the port's `peer` link (below) says which
   halves pair. The key is the native spelling (D2),
   so the twin is a different port, never an alias, and `usb_port` takes
   one spelling, not a list: accepting both halves would let one socket
   answer to two entries. With a list, the listed number becomes a placeholder and the
   camera is unlisted, never opened; with no list, its port-order
   position can change.

   What the driver owes the operator is the right reason. When a listed
   port has no working camera and a camera of this SDK sits on its twin,
   the placeholder's `Connect` error and `usb-devices.resolve` (`warn`)
   say *"a camera is on the USB 2.0 twin of this port — it negotiated
   USB 2 speed; check the cable"* rather than the generic reason. The
   pairing is read passively from data the collectors can hold: on
   Linux the port's sysfs `peer` link; on Windows the ACPI chain of the
   multi-valued `LocationPaths`, where `HSnn` / `SSnn` root-port names
   pair the halves. Where no pairing signal exists — macOS, or a hub
   that exposes none — the message falls back to the generic reason,
   and `usb-devices.unlisted` still shows the camera on its new
   spelling. No-list cameras get no twin hint: a USB 2-only camera in a
   USB 3 connector also lands on the companion, and nothing passive
   tells the two apart.

Note the deliberate asymmetry: **the list is port-keyed, while the
`devices` override map and the ASCOM `UniqueID` stay serial-derived —
for every camera that has a real serial.** `UniqueID` is a shipped
contract that rp and every ASCOM client stores; re-keying it on the port
would mean a device's identity changed when its cable moved, which is
exactly wrong for identity even though it is exactly right for a device
number. The two vocabularies answer different questions — *which socket
is this?* versus *which camera is this?* — and for a listed camera the
list's own `name`/`description` (and, in `qhy-camera`, the declared
wheel's `filter_names`) replace the override map, so the serial-keyed
map remains only for the no-list default, and a non-empty map next to a
list is rejected at load (D3).

**For a camera with no serial the two do meet, and the consequence must
be stated plainly.** D4.6 replaces the `noserial-{index}` fallback with
`noserial-{port}` — **in the two drivers that mint such a fallback**.
In `zwo-camera` and `svbony-camera` the minted string is *both* the
`UniqueID` suffix **and** the key of the `devices` override map
(`mint_identity` in each), so the change moves both: an existing
`devices` entry keyed `noserial-0` must be re-keyed to
`noserial-1-4.2` (or moved into a `usb_devices` entry), and the
`UniqueID` such a camera publishes changes with it.

**QHY is different and needs its own decision.** It mints nothing: the
raw `GetQHYCCDId` string *is* the `UniqueID` and the `devices` key
(`qhy-camera/src/camera.rs`). For models whose id carries a constant or
absent serial suffix, two such cameras therefore collide on `UniqueID`
today — a pre-existing defect this plan exposes rather than causes. With
every registered camera's port now known, a port-based identity for
exactly those models is trivially derivable; C5 still decides whether
QHY adopts it (with the matching `devices`-key and shared-CFW-id
migration) or documents the collision as known. It cannot be swept under
"same change for the other drivers".

**The breaking changes land together in C5, now.** C2–C4 add the list
path driver by driver and leave the no-list path as it is today (every
SDK camera, in SDK order); C5 then flips all three drivers at once:

- **No-list numbering by port order** (D4.7), replacing SDK order.
- **Unplaceable cameras refused under no list** (D4.3, D4.4) —
  look-alikes, unrecognised models, cameras whose record is a fault or
  that join no working record, cameras the driver fails to open, and
  every camera while the scan is failing.
- **Serial-less identity** `noserial-{index}` → `noserial-{port}` (D4.6).

The workspace is at 0.1.0 with no published CHANGELOG and a handful of
known rigs: this is the cheapest these changes will ever be, and each
fixes an ambiguity the code already documents as a flaw. Deferring would
ship the known-wrong behaviour into 1.0 and then break more people. One
disruption, one upgrade step: re-run `doctor --devices`, paste the
`usb_devices` block it prints, and fix rp's device numbers once. The
upgrade note lives in the C5 PR body and in each driver's design doc,
since there is no CHANGELOG to carry it.

**Migration note for C5: rp's roster pins.** rp gained an optional
`unique_id` on every roster entry, which refuses a connect when the
device at the entry's number reports a different `UniqueID`
([rp.md § Device Identity Pin](../services/rp.md#device-identity-pin)).
It is an interim guard for #1184, outside this plan, and it is only as
strong as each driver's `UniqueID`. QHY's is serial-derived and tells
two cameras apart. The `noserial-{index}` fallback in `zwo-camera` and
`svbony-camera` follows the enumeration index, so today a pin on a
serial-less camera cannot tell two of the same model apart. D4.6's
`noserial-{port}` changes those `UniqueID`s, so after C5 every rp pin
on a serial-less camera is stale and rp refuses it loudly. C5's upgrade
note must tell the operator to re-copy those pins from rp's bound-device
log line or from `GET /api/equipment`.

### D5. Doctor as the setup tool

The operator never types a port path from memory. Every catalog camera
service's existing `doctor` subcommand grows a device listing, printed
with a paste-ready `usb_devices` block.

This is **new plumbing, not a reuse**: the per-service probe today calls
`Sdk::new()` and the shared runner emits only config and SDK checks — it
never receives the passive `HardwareFacts` USB inventory, which is
gathered on the central-doctor path. Since `--devices` must print ports
*before* any SDK device-touching operation, each driver phase (C2–C4)
adds the passive collector (and its error handling, per D4.4) to that
service's per-service path — the join needs it there anyway — and ships
the listing with it, rather than extending the existing SDK listing. C5
adds the checks.

```
$ qhy-camera doctor --devices

Cameras on the bus for this driver (VID 1618), in port order

  Port     Model          SDK id                      Serial   Device
  1-4.2    QHY268M        QHY268M-b4a1f2ab3c4d5e6f    —        0
  1-4.3    QHY5III715C    QHY5III715C-0000000000      —        not listed

Pin the cameras this driver serves — paste into qhy-camera.json, leave
out any camera another application owns (PHD2's guide camera), and keep
the numbers running 0..N-1:

    "usb_devices": [
      { "device_number": 0, "usb_port": "1-4.2" }
    ]

  ok    usb-devices.resolve    1 listed port resolves, no placeholders
  ok    usb-devices.unlisted   1-4.3 (QHY5III715C) is on the bus and not listed
```

Model and SDK id are shown so the operator can tell which row is which
camera; neither is something they ever type. Rows are SDK cameras placed
on a port (D4.2) — never a non-camera record that shares the vendor id.

**What the block holds.** With no list configured, it lists every camera
the driver can place, numbered in port order — what the no-list default
is already serving. Each entry carries the `devices` override fields
that camera is served with today, so that pasting the block and deleting
the `devices` map it replaces (a map next to a list is rejected, D3)
changes no number and no name:

- Doctor matches a camera to its `devices` key without opening it: QHY's
  `GetQHYCCDId`, SVBony's enumeration serial, or `noserial-{port}` for a
  serial-less ZWO or SVBony camera. A camera with no override carries no
  `name`, so it keeps its SDK-derived default (for QHY, the full SDK id).
- A ZWO camera with a real serial cannot be matched without opening it,
  which `--devices` must not do. Doctor names every `devices` key it
  could not place and says to move its fields into the right entry by
  hand.
- A QHY CFW cannot be detected without `InitQHYCCD`, which `--devices`
  must not call either. A camera whose `devices` override carries
  `filter_names` gets `filter_wheel_number` and those names in its
  entry; for any other camera, doctor says to add `filter_wheel_number`
  if it has a wheel, because an undeclared wheel is not registered
  (D4.7).

With a list configured, the block reproduces the current list, every
entry and number included — placeholders too, so the numbers still run
`0..N-1` — likewise a no-op until edited. Unlisted cameras appear in the
table and in `usb-devices.unlisted`, never in the block: serving one is
an ownership decision, so the operator copies its port from the table
into an entry. For a moved camera, the old entry just takes the new
port.

That "no-op" holds from C5, when port order becomes the no-list
numbering. In C2–C4 a no-list driver still serves SDK order, so pasting
the block can renumber cameras — the C5 upgrade step taken early — and
rp's `cameras[].device_number` must be checked when it is pasted.

**The Serial column is the USB descriptor's serial, and it is usually
blank.** `--devices` is enumeration-only, so it cannot open a ZWO camera
to mint an SDK serial — and must not, least of all for a camera it does
not serve. What it prints is whatever the passive USB scan carries
(`serial` from sysfs / PnP), which cameras rarely publish (none of the
cameras on `rig2` or the dev box does, D4.2). The column is therefore
advisory: `—` means "not published on the bus", never "this camera has
no identity".

**The SDK id column is SDK-specific, for the same reason.** QHY's
`GetQHYCCDId` is available during passive enumeration, so it can be shown
for every QHY device, listed or not. **ZWO's is never shown** — not even
for a listed camera. `doctor --devices` is a separate, short-lived
process with no open handle of its own, so populating that column would
mean `ASIOpenCamera` from inside the setup tool, against a camera the
running service may be streaming. The rule is "print what this SDK
yields passively" — never open anything, and never hide an id that was
free to read.

Three checks join the per-service set in C5, alongside
`config.full-shape` and `hardware.sdk-devices`. They follow the rule
central doctor applies to USB ([doctor.md](../services/doctor.md),
"Hardware"): **a device that is on the bus but not working never fails
doctor, and a scan that could not run does.** These three checks
therefore fail only on a failed scan: a camera that is not working, a
listed port with no working camera, a look-alike or an unrecognised
model is information for the operator — `warn`. Central doctor keeps its
own rule for a device *absent* from the bus — `fail` through
`hardware.usb-device` for an installed, enabled unit — which a
per-service binary cannot apply, because it cannot see unit state (the
same reason `hardware.sdk-devices` only warns on zero devices). A list
that cannot load fails `config.full-shape`, as any invalid config does.

| Check | Trigger |
|---|---|
| `usb-devices.resolve` | With a list, per entry, judged from this process's own scan — what the service *would* serve if it started now, not what a running instance holds (placement is not watched, D8): its port resolves to a camera (`ok`); its port has no working camera — a placeholder serves the number (`warn`), giving the fault's reason when a fault's `location` is that port, the device that is there when the port holds a working record no SDK camera is placed on (pointing at `hardware.sdk-devices` and, for `qhy-camera` on Linux, `hardware.firmware-helper`), the speed fallback when a camera of this SDK is on the port's USB 2.0 twin (D4.8), and otherwise counting the faults the scan could not place; its port holds a look-alike or an unrecognised model (D4.3) — a placeholder serves the number (`warn`, naming the collision, or the SDK model and VID:PID); the USB scan failed — every listed number is a placeholder (`fail`, naming the collector error). An open failure (D4.4) is invisible here, because doctor never opens anything (D6): the driver's `warn!` and the placeholder's `Connect` error are its report. |
| `usb-devices.unlisted` | With a list: SDK cameras placed on a port that no entry lists, for information (`ok`) — so "why is my camera missing" answers itself, and a camera moved to a new port shows up here while its old number reports a placeholder. |
| `usb-devices.implicit` | No list. **Registered cameras:** when the driver would register more than one, names them with the numbers port order gives them and prints the paste-ready block of exactly those cameras (`ok`, informational); silent for one or none. **Refused cameras**, reported whatever the registered count: each look-alike, naming the collision; each unrecognised model, with its SDK model and VID:PID; each camera whose record is a fault, with its reason (`warn`); and when the USB scan failed, one `fail` naming the collector error and no block, because a failed scan has no ports to print. Refused cameras are never in the block (a listed look-alike would only become a placeholder), but the block of the placed cameras is still printed beside them. |

**On a running rig, placeholders reach the central report through
`service.devices`, not through these checks.** Per-service checks reach
a central report only through the shell-out, and that runs only while
the unit is inactive ([doctor.md](../services/doctor.md), "Aggregation —
the two probe paths"). With the unit active, central doctor reads
`configureddevices`, where a placeholder carries its entry's `name` and
would otherwise be listed `ok` as a served camera. C5 therefore teaches
`service.devices` the placeholder `UniqueID` form (D4.5): it reports
each placeholder as `warn` — *"Camera #1 is a placeholder for
PCIROOT(…)#USB(14)#USB(3), not a camera; run `qhy-camera doctor
--devices` for the reason"* — and leaves it out of the `ok` inventory.
If the host's own USB scan now holds a working device on that port (a
hub powered on after start), the detail adds that a reload would pick
it up. A running service cannot show look-alikes or cameras the no-list
default refused, because they are not registered at all;
`<svc> doctor --devices`, safe to run by hand at any time, is where they
are named.

An automated *drift* check ("this port used to hold a QHY268M") is
deliberately **not** in scope: the list pins a port, not a model, so
there is nothing in the config to compare against, and inventing a
remembered-state file to enable one check is not worth it. A moved cable
shows up as a placeholder `warn` on the old number plus an unlisted
camera on the new port — the listing above shows the operator both.

**The unplug procedure answers "which port is this camera in?"** When
the operator cannot tell which row is which physical camera, doctor
tells them to unplug one and re-run: the port that disappears is the one
they just unplugged. That is enough to *write the list*, and it is all
this procedure does. It does **not** rescue look-alikes (D4.3): two
identical cameras with no serial readable passively on both the bus and
the SDK side (for ZWO, any two of one model) offer no join signal once
both are back, so the driver still refuses both, and doctor says exactly
that rather than implying another re-run would help — *"two QHY5III715C
on 1-4.2 and 1-4.3 cannot be told apart; neither can be served while
both are connected to this host."*

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

// stage 2: probe (open → init → CFW → close) only the ids this driver
// serves; one outcome per id. The outer `?` is an SDK-level failure only.
let opened: Vec<(String, Result<Camera>)> = Sdk::open_selected(&selected)?;
```

A selected id whose open fails gets its own `Err` and becomes a
placeholder at its listed number (D4.5) — never silently dropped, as
today, where `cfw_probe` returns `None`, `Sdk::new` `continue`s, and
every later number shifts. With declared wheels (D4.7) a listed camera
needs no CFW probe at startup at all, so for the list path this stage
reduces to whatever the camera itself needs.

**Stage 2 is still not safe at startup, and this is the sharpest thing
in Part A.** Filtering to the cameras this driver serves stops the probe reaching
*other people's* devices, but it does not make the probe itself
permissible: `InitQHYCCD` is what may auto-home a connected CFW (measured not
to on a QHY178M + CFW3 or a QHY600M — `qhy-camera.md` C5), and
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

`qhyccd-rs` stays generic — it takes the set of ids to open; it never
learns about rusty-photon config. The service applies `usb_devices`
between the stages.

### D7. Per-driver work

**`svbony-camera` (C2)** — place each `CameraInfo` entry on a port before
registering, serve listed ports at their numbers, and register
placeholders for the rest. `CameraSN` is free at enumeration, but the
serial route of the join also needs the same serial on the *bus* side,
and whether SVBony cameras publish one is unverified — otherwise SVBony
joins on VID:PID and product string like the others, and two identical
SVBony cameras are look-alikes (D4.3). It is still the easiest driver to
prove the schema, the join and the placeholder behaviour against,
because nothing has to be opened to place a camera.

**`zwo-camera` (C3)** — with port-keyed placement the passive
`open_uninitialised()` is no longer part of deciding which camera to
serve at all. It runs only for cameras already placed on a listed port,
to mint their identity; with no list it keeps running for every SDK
camera, as today, until C5 places those too (see the breaking-change
note under D4). A ZWO camera on an unlisted port is now never opened,
where today every camera is. A failed `open_uninitialised` — a wedged
camera, or one another process such as PHD2 holds — becomes a
per-camera outcome: today's `?` in `enumerate_cameras` fails the whole
enumeration, so one such camera stops every other number. Under this
plan it is a placeholder at its listed number, naming the SDK error;
with no list the camera is not registered and is logged at `warn!`
(D4.4).

**`qhy-camera` (C4)** — the enumerate/probe split above, plus placement
and the declared filter wheels (D4.7).

### D8. Out of scope for Part A

- Filter wheels, focusers and rotators as devices of their own. The
  pattern generalises, but the motivating conflict is cameras, and
  `zwo-focuser`/`qhy-focuser` have no competing consumer today. Widen
  when a second consumer appears. (The CFW `qhy-camera` registers
  through a camera *is* covered, D4.7.)
- Hot-plug. Placement resolves at enumeration (start / reload) and is
  not watched. A camera plugged in later — including one a placeholder
  stands in for — appears only after a reload or restart, which
  rebuilds the server and re-opens **every** camera the service serves,
  so the operator picks the moment; the placeholder keeps the number,
  so nothing renumbers. The ways to trigger it are `systemctl reload`,
  `sc.exe control … paramchange` under the SCM, ui-htmx's restart via
  Sentinel, or restarting a console-mode process; a `config.apply` that
  changes nothing fires no reload, and Windows console mode has no
  reload signal. The only reload a driver fires on its own is the one
  after its failed USB scan recovers (D4.4). Recorded, not built: a
  placeholder whose own `Connect` re-resolves its port and becomes the
  live camera. `Connect` is an explicit operator action, so an open is
  permitted there, and no other camera would be touched; it needs an
  SDK re-enumeration while the service's other cameras are open, which
  is unproven for each of the three SDKs.
- Re-keying `UniqueID` on the port for cameras that have a serial, or the
  no-list `devices` override map (see the note under D4).
- A USB 3 camera's USB 2.0 twin as an alias of its listed port (D4.8).
- Resolving look-alikes by opening them (D4.3) — recorded, built only
  when a rig needs it.
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
  socket under live clients is not a reload. That runs into the
  per-driver `ApplyDisposition` limit (`config-actions.md`: one
  disposition per driver, no per-field one), so C6 either makes the
  whole facade restart-only or adds per-path dispositions to the shared
  config-actions API. (The device list of Part A needs neither: a
  changed `usb_devices` applies through the ordinary reload, D4.1.)

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
  `materialize_identity` at its own config pointer, as `sky-survey-camera`
  and the serial drivers do — the SDK camera services pass `&[]` only
  because they derive `UniqueID` from enumeration (D4.6), and the facade
  has no hardware of its own to enumerate. It is **not** a value derived from
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
| 1 | **The USB port path is the only device key** (D1) | Serial does not exist for every camera (the ASI1600 exposes neither serial nor flash id); three ways to name one device means three code paths and a config whose meaning depends on which key the author reached for. Serial and model stay as internal join signals and doctor display columns, never config surface. |
| 2 | **C1 is a blocking hardware spike** (D2) | The Windows port spelling was the one leg still unchosen; the Linux directory name was chosen first, but its bus number depends on controller registration order, so it gave way to a controller-anchored spelling (D2). Prove the Windows spelling on the real box — direct and behind a hub, across replug and reboot — before any schema commits to a spelling. An unstable key on one platform is worse than no key. |
| 3 | **The facade listens on 11128** (D10) | It joins the Alpaca device block because a port should say what a client finds there, and what is there is an ASCOM Camera. Its hosting process is not a client-visible fact. The port is a second listener under a new nested `camera.server` block — not a reuse of the existing REST `server` — and it brings catalog, packaging and firewall registration with it. |
| 4 | **`PixelSizeX`/`Y` come from config alone — as two fields** (D10) | ASCOM clients and ConformU read `PixelSizeX` right after connect, before any exposure, and tenet 3 forbids capturing a frame on connect to discover it. A FITS-header cross-check was dropped as a second source of truth for a value typed once per rig. `pixel_size_x_um` and `pixel_size_y_um` are separate because ASCOM and rp treat them as separate invariants; one value would advertise square pixels for a rectangular sensor. |
| 5 | **No list is the permanent default: every camera the USB scan can place, numbered by port order; cameras it cannot place are refused** (D3, D4.3, D4.4, D4.7) | No deprecation and no future release demanding the list: an existing file stays valid and single-camera rigs never meet the block. The default's behaviour still changes once, in C5 (row 6): a multi-camera no-list rig can be renumbered and look-alikes it serves today are refused, so rp's device numbers may need revisiting once (D3). Port order is stable while the set of cameras is unchanged. Refusing unplaceable cameras (look-alikes, unrecognised models, faults, a failed scan) keeps every served number tied to a socket, at the price of a failed scan leaving a no-list rig with no cameras until a background re-scan succeeds (row 14). Doctor's `usb-devices.implicit` finding nudges only the multi-camera case. |
| 6 | **The breaking no-list changes — port-order numbering, refusal of unplaceable cameras, and the serial-less `noserial-{port}` identity — land together in C5, at 0.1.0; C2–C4 leave the no-list path as it is today** (D4.3, D4.4, D4.6, D4.7) | Pre-1.0, no CHANGELOG, few rigs — the cheapest this will ever be, and each fixes an ambiguity the code documents as a flaw. One disruption, one upgrade step. |
| 7 | **An explicit `usb_devices` list pinning port → device number replaces `include`/`exclude`** (D3) — 2026-09-29 | The purpose is that a camera on a given port is the same ASCOM device every time; an operator who adds or moves a camera edits the config. One positive list makes ownership fall out (unlisted = never opened) without a deny-list whose failure mode is opening a moved guide camera, and without the resolver rules an exclude mode needed. Explicit numbers, not list position, so reordering entries renumbers nothing and no number moves unless the operator writes the change. |
| 8 | **A listed number whose camera cannot be served is held by a placeholder device** (D4.5, D4.7) — 2026-09-29 | Keeps numbers dense from 0 (start-at-0 per Alpaca §2.1.3; density for the fork and rp) with no change to the `ascom-alpaca-rs` fork and none to rp's binding, which takes a camera by its position among the server's cameras. rp only learns to treat the placeholder's fixed error code as permanent for the pass, so it does not retry it with backoff all night. Sparse numbering needed both changes and still broke start-at-0 when device 0 was the absent one. A client sees a camera that cannot connect tonight, with the reason in both the connect error and `Description`. |
| 9 | **Look-alikes are refused, never guessed** (D4.3) — 2026-09-29 | No SDK reports a port, the observed cameras publish no USB serial, and ZWO's SDK serial needs an open, so two cameras of one SDK on one host that are identical on the bus cannot be told apart passively. That covers two of one model and, for QHY, very likely the mono and colour variants of one sensor family. A model the normalizer does not know is paired by elimination when nothing else is left, and refused the same way when it cannot be. Resolving them by opening each and reading the OS-side handle is possible for cameras the driver owns, and is recorded but not built until a rig needs it. |
| 10 | **A changed list applies through the ordinary reload** (D4.1) — 2026-09-29 | An operator changing cameras is not mid-session, so closing and re-opening every camera on the service (its clients reconnect) is fine; no restart-only or per-path disposition is needed. Tenet 3 still binds every reload (C4). |
| 11 | **A listed camera's overrides live in its `usb_devices` entry** (D3) — 2026-09-29, extended 2026-10-01 | One place per listed camera: `name`, `description` and, in `qhy-camera`, the declared wheel's `filter_names` (row 13). The serial-keyed `devices` map remains only for the no-list default, and a non-empty map next to a list is rejected at load rather than silently ignored. |
| 12 | **A USB record that is not a working device is a fault, never a failed scan** (D4.4) — 2026-09-29, #1322 | One permanent Windows enumeration-failure placeholder blanked every presence answer on `rig2`. Faults are reported and left out of the inventory; only the collector itself failing fails the scan. A fault never fails doctor, and a failed scan always does. The per-service `usb-devices.*` checks fail only on a failed scan — a fault, an empty listed port, a look-alike or an unrecognised model only warns (D5) — while central `hardware.usb-device` keeps failing a device absent from the bus for an installed, enabled unit. |
| 13 | **A `qhy-camera` entry declares its camera's filter wheel** (D3, D4.7) — 2026-10-01 | rp binds filter wheels by position, so a wheel numbered in camera order would shift behind a placeholder camera and rp would move another camera's filters without error. `filter_wheel_number` pins the wheel as `device_number` pins the camera, a placeholder camera's wheel is a placeholder too, and a declared wheel needs no startup `InitQHYCCD` probe. |
| 14 | **A failed scan is retried in the background and the driver reloads itself on the first success** (D4.4) — 2026-10-01 | The Windows collector's 10 s deadline can run out on a healthy host at boot, and a failed scan is never a startup failure, so nothing else would retry it before morning. Safe because the failed-scan outcome has opened nothing; not hot-plug, because it stops at the first success. |
| 15 | **A camera on a port's USB 2.0 twin is on a different port, and the placeholder says why** (D4.8) — 2026-10-01 | The native spelling is the key, and one socket answering to two entries would undo the list. Where a passive pairing signal exists the reason points at the cable instead of the generic one. |
| 16 | **`usb_devices` numbers must run `0..N-1`; a gap is rejected, not filled** (D3) — 2026-10-01 | Every served number is one the operator wrote, so a typo cannot become a phantom camera. Retiring a camera below the highest number means renumbering the entries above it (and rp); a retired camera's entry is never kept to hold a number, because a listed port opens whatever is plugged in there next. |

Nothing in this plan is waiting on an *operator* answer. **Five** things
are waiting on evidence or an implementation choice, each named at its
rule — three of them block a phase outright (C4's tenet-3-safe probe
path, the macOS `system_profiler` check before C5's no-list flip, and
C7's focus-model reconciliation). C1's last wait, a move to a different
port, was answered on a Windows VM on 2026-10-10 (D2, spike item 7), so
nothing now blocks C2.

- **C6 — the capture completion watermark**, waiting on one measurement
  against a live PHD2 (D9). Until it exists the facade cannot tell a
  finished exposure from the frame before it.
- **macOS — whether `system_profiler -json SPUSBDataType` still works on
  macOS 26.** Reported removed there, unverified. Since a failed scan
  leaves a no-list rig with no cameras (D4.4), this needs checking on a
  current Mac before C5 flips the no-list default.
- **C4 — a tenet-3-safe CFW detection path**, waiting on a choice
  between deferring the probe to client connect, finding a non-actuating
  detection call, or an explicitly-unsafe operator command (D6). A listed
  camera's declared wheel (D4.7) already takes the first option; the
  choice is what the no-list path does. Blocks C4: the enumerate/probe
  split alone does not discharge the tenet.
- **C5 — QHY's serial-less identity policy**, waiting on a choice
  between giving those models a port-based identity (with the matching
  `devices`-key and shared-CFW-id migration) and documenting the existing
  `UniqueID` collision as known (D4.6).
- **C7 — its relationship to [`focus-model.md`](focus-model.md) S7/D17**,
  which retires rp's capture-based `auto_focus`. C7 must not land before
  that is settled, or the two plans race on one contract (D12).

# Plan: ZWO real-hardware validation on the field rig

**Date:** 2026-07-11
**Branch:** `feature/rig-dev-loop`
**Status:** waiting on hardware (ZWO color main camera + ZWO EAF focuser,
expected within days). No filter wheel is in the order — zwo Phase F
(EFW/FilterWheel, `docs/plans/zwo-driver.md`) stays out of scope here.
**Context:** both ZWO services have only ever been validated against the
simulated SDK: zwo-camera passed ConformU 2026-06-16 with the *sim* backend;
zwo-focuser (merged as PR #479) has **never touched a real EAF**. The field
rig already runs both services (ADR-014 per-device debs, empty device lists)
plus the full stack, so validation is configuration + observation, not
deployment work. Remote dev loop: see
[docs/skills/rig-development.md](../skills/rig-development.md).

---

## 1. Arrival-day checklist

### 1.1 Physical / enumeration

- [ ] Camera **directly on a Pi USB3 port**, not the PPBA Gen2's built-in
      hub — on 2026-07-11 the QHY5III715C's firmware reproducibly failed to
      boot on the PPBA's external USB3 ports (raw device flashed, then
      vanished before enumerating) while working elsewhere; treat those
      ports as serial/low-speed-only until proven otherwise. Verify
      SuperSpeed enumeration: `lsusb -t` must show the camera at 5000M on
      bus 2/4, not 480M on bus 1/3.
- [ ] EAF connected (USB2 is fine — it's a low-rate HID-class device).
- [ ] `lsusb` shows ZWO vendor ID `03c3` for both devices.
- [ ] Camera 12 V (cooler) fed from a PPBA switched output; note which
      output in the PPBA switch names.

### 1.2 Permissions / udev

- [ ] The zwo debs' udev rules fire: device nodes accessible to group
      `plugdev` after replug (`udevadm info` on the device; the service user
      is already in `plugdev` via the unit's `SupplementaryGroups`).
- [ ] `systemctl restart rusty-photon-zwo-camera rusty-photon-zwo-focuser`;
      both list their device (`/management/v1/configureddevices`).

### 1.3 zwo-camera against real hardware

- [ ] First real-SDK ConformU run against the rig's zwo-camera, invoking
      ConformU directly as
      [hardware-validation.md](../skills/hardware-validation.md#install-and-run)
      says. The sim round surfaced sensor-geometry and async-op bugs
      (CameraXSize R4 alignment, async PulseGuide); expect the real sensor
      to disagree with the sim on geometry, exposure limits, and gain/offset
      ranges — capture every mismatch as a driver issue rather than
      hand-tuning config around it.
- [ ] Full-frame capture at 16-bit; verify FITS lands via filemonitor and
      dimensions/bit depth match the sensor spec.
- [ ] Cooler: set-point reached and reported power plausible.

### 1.4 zwo-focuser against real EAF (first contact ever)

- [ ] ConformU Focuser suite against the rig instance.
- [ ] Movement sanity: small relative moves in both directions, position
      readback consistent, no missed steps at travel extremes; confirm
      configured `max_step` against the EAF's actual range.
- [ ] Temperature sensor reads plausibly.
- [ ] Note backlash behavior (needed later for autofocus work).

### 1.5 Stack integration

- [ ] Add both devices to `ui-htmx` drivers, sentinel monitors, and rp
      equipment on the rig; confirm the `/equipment` roster tiers them
      correctly.
- [ ] `scripts/rig.sh fetch-configs` afterwards so local dev configs include
      the new endpoints.

## 2. Known risks

- **SDK pin vs new camera model.** The bundled `libASICamera2` comes from
  the pinned indi-3rdparty ref in `scripts/build-packages.sh`; a
  just-released camera model may need a newer blob. Symptom: camera
  enumerates on USB but the SDK lists no device. Fix: bump the pin (and
  SHAs) per ADR-013/ADR-014, rebuild the zwo debs on the rig.
- **USB3 on the Pi 5 behind hubs** has real-world flakiness; if SuperSpeed
  enumeration is unstable, try the Pi's own USB3 ports before the hub.
- **Power browning out the hub** during cooler + exposure load — the reason
  the rig uses a powered hub; watch `dmesg` for USB resets during first
  long-exposure tests.

## 3. Exit criteria

Both ConformU suites green against real hardware, one full-frame capture on
disk, EAF moving reliably, devices integrated into ui-htmx/sentinel/rp, and
each service's design doc updated with a "validated on real hardware" note
(zwo-camera.md, zwo-focuser.md). Then archive this plan per
docs/skills/archiving-plans.md.

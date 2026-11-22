# zwo-focuser: the first run against a physical EAF, 2026-07-14

A record, moved whole from `docs/services/zwo-focuser.md` on 2026-11-21:
the account of zwo-focuser's first run against a physical EAF, on a Linux
dev box on 2026-07-14, a run with no record in
[`docs/validation/`](../../validation/README.md). It stood under
[zwo-focuser.md § Real-hardware validation](../../services/zwo-focuser.md#real-hardware-validation);
that document states what the hardware does under the sections the run
names (*Hardware Constraints*, *Behavioral contracts* and *Packaging*),
and lists the focuser's validation records under § Real-hardware
validation.

**Performed 2026-07-14** against a physical EAF with the plug-in temperature
probe attached, on a Linux dev box (`cargo run -p zwo-focuser`, real build, no
`--features simulation`), driven over the Alpaca HTTP API and by ConformU
4.3.0. Results, in the order the original checklist posed them:

- **Linking and enumeration**: `libEAFFocuser` resolves; the unit enumerates
  and registers as device 0 with UniqueID `ZWO:EAF:<16-hex-serial>` —
  `EAFGetSerialNumber` works on this firmware, so the `noserial-0` fallback
  stayed unexercised.
- **Device access is the real prerequisite**: the EAF is a USB **HID** device;
  with `/dev/hidraw*` inaccessible, startup fails at `EAFOpen` with
  `EAF_ERROR_REMOVED` even though USB enumeration works. See *Hardware
  Constraints* and *Packaging*.
- **MaxStep**: the unit reports an `EAF_INFO::MaxStep` ceiling of **600000**
  but a firmware working limit (`EAFGetMaxStep`) of **60000** — and the motor
  silently stops at the working limit when commanded past it (ConformU caught
  this as a Move issue against the original `EAF_INFO`-based implementation).
  The driver now reports/validates the working limit; ConformU passes with
  zero errors, warnings or issues.
- **Movement timing**: ≈ 640 steps per second (≈ 64-65 steps per 100 ms
  poll); `IsMoving` stays true across many polls (1000 steps ≈ 1.7 s) with
  `Position` ramping live in between.
  The simulation's original settle-after-one-poll / jump-to-target model was
  materially wrong and now mirrors the measured behavior (see *Behavioral
  contracts*).
- **Halt**: `EAFStop` halts a real in-progress move, freezing the position
  mid-travel; a second `Move` while moving is rejected by the firmware
  (`EAF_ERROR_MOVING` → `INVALID_OPERATION`).
- **Temperature**: 20.5 °C on the bench — ambient-plausible (probe reading).
- **Position persistence**: the step counter survives power cycles (stored in
  the focuser, not the host).

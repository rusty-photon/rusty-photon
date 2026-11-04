# Daily power cycle — devices off when they are not needed, back on when they are

## Goal

A remote rig runs with as many devices as possible powered off whenever
nothing needs them, and powers them back on when a session does. The
cooled camera is the sharpest case, since its fan should not run all day,
but the focuser hub, the flat panel, the mount and the guide camera are the
same. So a device that is off for most of the day is the normal case, not
an error. The stack has to notice a device coming back without a service
restart, a reload or a person, and nothing may log in a loop or give up
while a device is off.

Deciding when to switch power is a workflow's job (decision 1), and the
services start with every device powered (decision 2). What this plan
builds is everything around those: a survey that each driver picks its
device up when its power returns, quiet logging while a device is off, a
guard for a camera's first exposure after power-up, rp tools that switch a
power-box output, the dusk and dawn documents that use them, and a doctor
that does not report the daytime state as a fault.

Tracking issue:
[#1173](https://github.com/rusty-photon/rusty-photon/issues/1173).

Out of scope: re-cabling a device onto another port, which stays a reload's
job, and a device that is off when its service starts (decision 2).

## What a power cut does to a device

The rig switches devices through a power box: its 12 V outputs, and its
switchable USB ports for whatever hangs off the box's hub. A device plugged
straight into the host keeps its USB power, so often only its 12 V is cut.
A cut does one of two things to a device. Which one depends on where the
device's USB interface draws its power, not on which port can be switched:

1. **It leaves the bus**, because its USB interface runs off the supply that
   was cut. The host sees a disconnect even on a port nobody switched, and
   the OS reports a removal, then an arrival when power returns.
2. **It stays on the bus with its function dead**, because its USB interface
   runs off USB power. Nothing has to be found again, but its driver has to
   keep serving it while it does not work and pick it up when power returns.

| Device | 12 V cut, USB left on | Source |
|---|---|---|
| QHY178M (dev box) | Stays on the bus, answers every call, reports its cooler as present. Its filter wheel, fed from the camera's 12 V, homes for about 20 s when the 12 V returns | [Linux](../validation/2026-10-05-qhy-camera-qhy178m-cfw-linux-departure/README.md) and [Windows](../validation/2026-10-06-qhy-camera-qhy178m-cfw-windows-departure/README.md) departure records |
| QHY600M (rig2) | Leaves the bus, and comes back through its firmware loader about 1 s after its 12 V returns | the Windows departure record above; Validation |
| Optec FocusLynx hub (rig2) | Leaves: its COM port appears only once the hub has power | the tracking issue's 2026-09-06 measurement |
| DSD FP2 (rig2) | Its COM port was gone with the power box off; whether its USB was on a switched port is not recorded | the tracking issue's 2026-09-06 comment |
| ZWO cooled cameras, SVBONY, Pegasus Falcon and Scops, QHY focuser, Star Adventurer GTi | Not measured | P0 |

A switched USB port takes any device off the bus. The ZWO departure records
were made by switching the PPBA's `USB Hub` output, and the QHY probes below
by switching the Gemini Advanced 3's `USB A` and rig2's UPBv2 12 V output 1.

## Where the stack stands

- **The camera drivers and `zwo-focuser` build their device list once**, at
  start and at each reload. The Alpaca server library (the pinned
  `ascom-alpaca-rs` fork) moves the list into an `Arc` when it builds its
  router, so no device can be added or replaced while a server runs. With a
  `usb_devices` list, a camera absent when the list is built is a
  placeholder at its number (`UniqueID` `placeholder:<service>:<port>`,
  `Connected = true` refused with `0x540`) until the next reload. Without a
  list it is not served at all. `zwo-focuser` has no list and serves the
  EAFs the SDK enumerates. A camera that was served, left the bus and came
  back opens again on a plain `Connected = true`, on all three camera
  drivers (measured).
- **The serial drivers run an eager startup handshake** before binding
  their listener: `dsd-fp2`, `ppba-driver`, `upbv2-driver`,
  `gemini-powerbox-adv3-driver`, `qhy-focuser`, `pa-falcon-rotator`,
  `pa-scops-oag` and `star-adventurer-gti`. A port that is absent, or a
  device that does not answer, fails the start, and a reload with the device
  absent ends the process. The units restart on failure every 5 s with no
  start limit, so a boot or an upgrade while a device is off loops until the
  device has power (rig2, 2026-09-06: 608 starts in 69 minutes). Once
  running, the shared transport's supervisor retries a lost link every 5 s
  and never gives up.
- **rp's reconnect supervisor** retries every configured device every
  `equipment.reconnect_interval` (30 s by default) and never gives up. A
  placeholder's refusal costs one attempt, logged at `debug!`. Any other
  failed connect is tried three times within the pass, with two `INFO`
  lines, so every powered-off device that is not a placeholder logs twice
  every 30 s.
- **rp has no switch tools.** `switches[]` is roster membership and
  connectivity only. `ppba-driver`, `upbv2-driver` and
  `gemini-powerbox-adv3-driver` expose their outputs as ASCOM Switch ids,
  each with an operator label.
- **doctor** fails `hardware.usb-device` for an enabled service whose
  device is absent from the bus.
- **Linux serial device names follow enumeration order** (`/dev/ttyUSBn`,
  `/dev/ttyACMn`), and every power-up enumerates the bridge chip again. Only
  a `/dev/serial/by-id/…` or `by-path/…` link names the same device across
  power cycles. A CH340 publishes no serial, so two of them share one by-id
  name and need `by-path`.

## Decisions

1. **Power is switched by a workflow** (settled 2026-11-03): a
   `session-runner` document or an operator's tool call. Never by a driver,
   a connect path, rp's reconnect supervisor or any other passive
   transition (tenet 3, *no actuation on connect*). Noticing that a device
   has come back is a read, and may run anywhere.
2. **The services start with every device powered** (settled 2026-11-04).
   An operator starts or restarts the rusty-photon services, at install,
   upgrade, reboot or by hand, with every device powered, USB and 12 V, and
   switches devices off only once the services have started. Every device is
   then found at start, and what is left is a device that leaves and returns
   while its driver runs, which the drivers already handle. A camera that
   was served, left the bus and came back opens again on a plain
   `Connected = true` on all three camera drivers. A serial driver's
   transport supervisor retries a lost link every 5 s and never gives up
   (Where the stack stands; P0 confirms it device by device). The Linux and
   Windows packaging guides tell operators so.
   - **On Windows it is the only reliable way for QHY.** The SDK lists a
     camera that was present at the process's first `InitQHYCCDResource`,
     and opens it again by id after any power cycle (7 of 7 on rig2). One
     that arrives later is listed only if the SDK's own arrival listener
     catches it, and on rig2 that listener missed 13 of 30 arrivals. A missed
     camera stayed invisible to the process until its next arrival, and an
     in-process Release + Init did not help (Validation). Under the rule
     every service's first init is the case the measurements cover: made at
     start, with the cameras present.
   - **A device that is off when its service starts**, after a daytime
     reboot or upgrade or a crash restart, is served as it is today. A
     listed camera is a placeholder, and an unlisted one is not served, until
     its service restarts with it powered. A serial driver fails its start
     every 5 s until its device answers, and then starts on its own.
   - **Not a device that binds when it arrives.** The alternative, settled
     on 2026-11-03 and dropped on 2026-11-04, registered every listed camera
     unbound from the start, with its identity stored in its list entry, and
     bound it on a client's connect or on the OS's USB hotplug notification;
     the serial drivers would have served an absent device disconnected
     instead of failing their start. All of it handled a device that is off
     at start, which the rule removes, and for QHY on Windows the binding
     would still have depended on the SDK's arrival listener.

## Open questions

Each is settled before the slice that needs it.

- **O1 (P1). rp's log for a powered-off device.** Proposed: rp logs that a
  device is unreachable at `INFO` once, on the transition, and at `debug!`
  while it stays unreachable, for every device and driver. A device that
  was unreachable on the previous pass gets one attempt, not three: rp walks
  the devices one after another, so 3 s of backoff per powered-off device
  would delay every other device's check. rp knows that from its own record,
  so this needs no trust in a driver's error code. Today's placeholder rule
  avoids that trust by checking the `UniqueID` form, because any Alpaca
  driver may use `0x540`.
- **O2 (P2). The first exposure after a power-up.** On rig2, 3 of 22
  QHY600M exposures started 3–9 s after the camera came up read out for
  60–66 s and returned an all-zero frame. The close that followed hung, and
  only a power cycle cleared the camera. None of 14 started 30 s or more
  after power-up wedged (Validation). The guard has to be one that every
  path powering a camera on meets: the dusk document, an operator's switch
  call, a recovery power cycle. Proposed: the camera driver holds the first
  exposure until its camera has been back long enough, since it sees the
  camera return whatever switched it. P0 measures whether the delay is what
  matters, how long it must be, and which other cameras need it.
- **O3 (P4). Readiness at dusk for a device that stays on the bus.** rp
  emits `equipment_changed {connected: true}` only for a device that was
  disconnected. A QHY178M whose 12 V was cut stayed connected all day, so
  the dusk document needs another signal that its cooler and wheel have
  power.
- **O4 (P4). When dusk starts.** At an offset from civil twilight and with
  the roof open, or the camera earlier so that it cools before the roof
  opens.
- **O5 (P5). doctor for a device off by design.** A daytime-aware reading
  of the checks a powered-off device fails or warns on
  (`hardware.usb-device`, `hardware.serial-node` for a COM port that leaves
  with its device, `usb-devices.resolve` for a listed camera that is off),
  or a "powered down by schedule" state.
- **O6 (P3). Which output feeds which device.** rp's `switches[]` entries
  name a power box, not the devices on its outputs, so P3's refusal to
  switch off an output whose device is busy needs a mapping from each output
  to the devices it feeds, in rp's configuration or in a train's.
- **O7 (P4). Driver state under a device whose 12 V is cut while it stays
  connected.** A QHY178M whose 12 V is cut stays on the bus and keeps its
  session all day. When the 12 V returns its filter wheel homes to slot 0
  for about 20 s, but `qhy-camera` answers a settled wheel's `Position` from
  the slot it cached and does not resend the slot it last commanded (FW1,
  FW2), and the wheel drops a command sent while it homes. So the first
  filter move at dusk can be skipped or lost, and the frame taken through
  the wrong filter. The options:
  - **(a) The power documents disconnect each device** on an output before
    switching it off, and connect it again once it has power and has
    settled. rp keeps such a device out of its reconnect supervisor in
    between, which needs O6's mapping. A fresh connect re-reads every cache
    its handshake fills (`qhy-camera` C6), and its
    `equipment_changed {connected: true}` doubles as O3's readiness signal.
  - **(b) The drivers re-read what they cache when a device's power
    returns.** With only the 12 V cut, no USB event tells them.

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| P0 | Power-cut survey: for each device on the dev box and rig2, cut its power the way the rig does (its 12 V, or its switched USB port) while its driver runs, and record whether it stays on the bus, whether it still answers, what stops working, and whether its driver picks it up when the power returns. Fills the table above and measures O2's delay | QHY178M probed on Linux and in a Windows guest, rig2's QHY600M on bare-metal Windows (Validation); the rest not started | |
| P1 | rp logs a powered-off device once, not every pass (O1) | Not started | |
| P2 | Camera drivers guard the first exposure after their camera returns (O2) | Not started; needs P0's measurement | |
| P3 | rp `get_switch` / `set_switch` tools, addressing a power-box output by its name or label, and refusing to switch off an output whose device is busy | Not started; needs O6 | |
| P4 | `session-runner` dusk and dawn documents. Dusk: outputs on, wait for readiness, `start_cooldown`. Dawn: `start_warmup`, wait for `cooler_warmup_complete`, park and cover, outputs off | Not started; needs P3, O3, O4, O7 | |
| P5 | doctor's reading of a device off by design (O5) | Not started | |

P0 and P1 depend on nothing else. P2 follows P0, which measures the delay
it needs, and P3 comes before P4.

`gemini-powerbox-adv3-driver` has a start bug of its own: one reply that
does not decode as UTF-8 during its handshake fails the start. It was seen
once, on the dev box's first start on 2026-11-03, the retry 5 s later was
clean, and it is filed as
[#1700](https://github.com/rusty-photon/rusty-photon/issues/1700).

## Validation

The QHY probes below were made while the plan still had a device that binds
when its camera arrives, dropped under decision 2, so most of their rows ask
about a camera absent at the SDK's first init. The rows on a camera present
at the first init and then power-cycled are the ones decision 2 rests on.

### QHY, Linux (2026-11-03)

The dev box's QHY178M (`QHY178M-222b16468c5966524`) on the Gemini
Advanced 3's `USB A` (its VIA VL817 hub), Fedora 44 (kernel 7.2.11), QHY SDK
26.06.04, with the camera's 12 V off throughout. The probe was a hand-run
program over `qhyccd-rs` and `libqhyccd-sys`, not the service. It switched
`USB A` through `gemini-powerbox-adv3-driver` and watched sysfs and
`udevadm monitor` for the camera.

| Question | Result |
|---|---|
| What powering `USB A` on looks like | 7 of 7 clean. The firmware loader (`1618:c178`) appears on `1-1.1` (USB 2) about 1.0 s after power-on, the kernel removes it once `fxload` has run, and the camera (`1618:c179`) appears on `2-1.1` at 5000 Mbit/s about 0.3 s later |
| Does `OpenQHYCCD(id)` open a camera that was absent at `InitQHYCCDResource`, with no `ScanQHYCCD`? | Yes, 3 of 3. An open 35 ms after the camera appeared failed; the next, 500 ms later, opened it, and its frame was good |
| Does `ScanQHYCCD` on a live SDK disturb an exposure, scanning between calls on the capture thread? | No. 3 exposures of 3 s with 18 scans each; frames and timings matched the controls |
| … scanning from a second thread through the exposure and the blocking readout? | No. 4 exposures of 3 s with 25 scans each. Every scan took under 1 ms and returned the same camera |
| Does scanning every 250 ms during power-up disturb the firmware load? | No. The scan reported the camera as soon as it appeared, and it opened and exposed normally |
| An open handle power-cycled with nothing touching it while off (0.15 s and 10 s off) | Clean. The first call after the camera returned failed at once (no stall, no blank frame), the close succeeded, and a reopen by id with no scan exposed normally |
| An open handle that a call reaches while the camera is off | A raw `CurTemp` read returned −722.2 °C instead of failing; the close and the reopen after power returned were clean. The service's `CCDTemperature` checks the cooler's availability first, which the departure contract treats as a presence question; that path is not yet exercised through the service |

### QHY, Windows (2026-11-03)

The same camera passed through to the dev box's Windows 11 KVM guest as a
USB `hostdev`, with QHY SDK 26.06.04 (`qhyccd.dll` 26.6.4.16) beside the
probe, built in the guest from the same source. A power-off was `USB A`
off followed at once by a detach from the guest; a power-on was `USB A` on,
then an attach once the host showed the camera. So the host's `fxload`
loaded the firmware, and the guest only ever saw the loaded camera
(`1618:c179`). The probe ran as `SYSTEM` through the guest agent (session 0,
as a service runs), except four runs as a user over SSH, and read the
guest's device tree through the Configuration Manager API.

| Question | Result |
|---|---|
| Camera absent at the process's first `InitQHYCCDResource`, then arriving | In 14 runs the SDK reported it in process 6 times: three times on its own, 4–8 s after Windows had the device running, and three times only after a Release + Init, 0.5–16 s after the re-init. The other eight runs saw nothing within 12–55 s. No `ScanQHYCCD` or `OpenQHYCCD` saw the camera before the SDK's `RegisterPnpEventIn` callback fired, and every in-process re-init enumerated zero cameras. The SDK's device listener watches WMI for departures only (`__InstanceDeletionEvent … Win32_PnPEntity`) and takes arrivals from a hidden window's `RegisterDeviceNotification` |
| The same camera, in a new process | Enumerated at the init, 7 of 7 |
| `ScanQHYCCD` on a live SDK during exposures | No effect. 4 exposures of 3 s with 18 scans each on the capture thread, and 4 with 26 scans each from a second thread through the readout |
| An open handle power-cycled (12 s off), the camera present at the first init | Clean. The first call after the camera returned failed at once, the close succeeded, and a reopen by id opened it on the first attempt, 3 s after it returned |
| An open handle that a call reaches while the camera is off | A raw `CurTemp` read returned 0.0 °C instead of failing (−722.2 °C on Linux) |

### QHY, Windows bare metal (2026-11-04)

rig2: an Intel NUC on Windows 11 Pro (build 26200). Its QHY600M
(`QHY600M-3b1ea54688eb9f9d7`, with a 7-slot wheel) sits on the UPBv2's USB
port 1 and was switched through `upbv2-driver` by its 12 V output 1, which
takes it off the bus. Its QHY5III678M (`QHY5III678M-4a94be45b27d3aa79`) sits
on USB port 3, whose power the UPBv2 switches. Windows' own QHY driver,
from QHY's All-in-One pack, loads the 600M's firmware. Two SDK builds were
used: the All-in-One's `qhyccd.dll` 24.1.9.12, which the installed service
loads, and 26.06.04 beside the probe. The probe is the one from the KVM
runs, built in the guest, with `qhy-camera` stopped while it ran. It ran as
`SYSTEM` from a scheduled task (session 0, as a service runs), except two
runs over SSH, both of which missed the arrival.

| Question | Result |
|---|---|
| What restoring the 600M's 12 V looks like | Clean every time. Windows starts the firmware loader (`1618:c600`) about 0.3 s after the 12 V, and the camera (`1618:c601`) about 0.6 s later; no SDK is involved |
| Camera absent at the process's first `InitQHYCCDResource`, then arriving (one camera) | 26 runs. In 16 the SDK's `RegisterPnpEventIn` callback named it 30–80 ms after Windows started the device, and `ScanQHYCCD` then listed it and `OpenQHYCCD` opened it. In the other 10 no callback came, and no scan or open saw the camera for the rest of the run: 15–30 s, plus two Release + Init rounds of 20 s in four of them, every one enumerating zero cameras. 24.1.9.12 kept 14 of 19 arrivals, 26.06.04 kept 2 of 7. Arrivals were missed with the process scanning and opening every second, with it waiting for the callback, with it scanning every 20 ms during the firmware load, and 16 s as well as 1.2 s after the init. Misses came in clusters: the first four runs all missed, and 12 of the 14 in the next 20 minutes kept theirs. The 8 runs that made one call, or a call every 5 ms, as the device started all kept theirs, but all of them fell in those 20 minutes. The four arrivals in which Windows was seen taking 50 ms or more to start the device were all missed |
| A second arrival in the same process, after a missed one | Reported both times: the callback fired and the camera opened |
| The same camera in a new process | Enumerated at the init, 9 of 9 |
| The 600M arriving while the 678M, present at the init, is open on the same SDK | Reported and opened 1 of 2 times with the 678M idle and 0 of 2 while it exposed back to back; the 678M's 84 frames were all good |
| The callbacks for a camera present at the init | `RegisterPnpEventIn` fires for it during the init. When its 12 V is cut, `RegisterPnpEventOut` fires 0.6–0.9 s after Windows drops the device; when the 12 V returns, `RegisterPnpEventIn` fires 35–80 ms after Windows starts it |
| An open handle power-cycled (5 s off), the camera present at the first init | 7 of 7 clean, on both SDK builds, with nothing touching the handle while off or with a call and a close while off. The first call after the return failed at once, the close succeeded, and a reopen by id 3 s or 30 s after the return opened it on the first attempt. The first exposure after one of them wedged (last row) |
| `ScanQHYCCD` from a second thread during the 600M's exposures | No effect. 2 exposures of 2 s with 18 scans each through the 1.5 s readout; every scan took under 1 ms and the frames matched the controls |
| The first exposures after a power-up | 3 of the 22 full-frame exposures started 3–9 s after the camera came up read out for 60–66 s and returned an all-zero frame. The close or SDK release that followed hung until the process was killed, and only a power cycle cleared the camera: a new process's first exposure on it wedged the same way. None of the 14 started 30 s or more after power-up wedged |

### Owed

- The P0 survey of every other device.

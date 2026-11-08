# Daily power cycle — devices off when they are not needed, back on when they are

## Goal

A remote rig runs with as many devices as possible powered off whenever
nothing needs them, and powers them back on when a session does. The
cooled camera is the sharpest case, since its fan should not run all day,
but the focuser hub, the flat panel, the mount and the guide camera are the
same. So a device that is off for most of the day is the normal case, not
an error. The stack has to bring a device back when its power returns
without a person, and nothing may log in a loop or give up while a device
is off.

Deciding when to switch power is a workflow's job (decision 1), the
services start with every device powered (decision 2), no device's session
crosses a power cut (decision 3), the serial drivers find their device
again after a cut nobody planned (decision 4), and every service whose
device was off restarts at dusk (decision 5). A powered-off device is
logged once (decision 6), dusk starts at civil twilight once the roof is
open (decision 7), rp's power-box entries map each output to its devices
(decision 8), doctor reads a device's output before it reports the device
absent (decision 9), `phd2-guider` starts PHD2 in the operator's session
(decision 10), and dusk checks each camera for a wedge (decision 11).
What this plan builds is
everything around those: a survey of what each device does across a power
cut, quiet logging while a device is off, services that restart on request,
rp tools that
disconnect and connect a device and switch a power-box output, the dusk and
dawn documents that use them, and a doctor that does not report the daytime
state as a fault.

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
| ZWO ASI1600MM-Cool (dev box) | Stays on the bus and exposes normally. Its cooler stops with no sign of it: `CoolerOn` stays true and `CoolerPower` climbs while the sensor warms, and it cools again as soon as the 12 V returns | Validation |
| ZWO ASI178MM and ASI120MC-S (dev box) | Have no 12 V input. Only their switched USB ports power them off, and then they leave the bus | Validation |
| QHY600M (rig2) | Leaves the bus, and comes back through its firmware loader about 1 s after its 12 V returns | the Windows departure record above; Validation |
| Optec FocusLynx hub (rig2) | Leaves: its COM port appears only once the hub has power | the tracking issue's 2026-09-06 measurement |
| DSD FP2 (rig2) | Its COM port was gone with the power box off; whether its USB was on a switched port is not recorded | the tracking issue's 2026-09-06 comment |
| DSD FP2 (pier1) | Leaves the bus. It shares the PPBA's quad 12 V output with the mount, and is back about 0.25 s after the mount when the output returns | Validation |
| Star Adventurer GTi (pier1) | Leaves the bus within about 0.2 s and is back about 0.55 s after its 12 V returns, its controller restarted with its encoders at 0 | `star-adventurer-gti`'s [§Power cut while serving](../services/star-adventurer-gti.md#power-cut-while-serving) |
| QHY5III715C (pier1) | Has no 12 V input and sits on a host USB port, so no output the rig switches reaches it | Validation |
| Pegasus Falcon Rotator (pier1) | Stays on the bus with its motor dead: the motor runs off the quad output, the controller off USB. A move sent during the cut is accepted with no error and reads finished at once, with the rotator where it was, and nothing moves when the 12 V returns | Validation |
| Pegasus Scops OAG (pier1) | Not fed by the quad output: it moves through that cut. The PPBA's `USB Hub` switch takes it off the bus | Validation |
| ZWO EAF (pier1) | Powered from USB, on a port of the PPBA's hub that no switch reaches | Validation |
| SVBONY SV605CC (pier1) | Its cooler kept cooling through the quad cut. It draws from the PPBA's Adjustable Output, the only other output carrying a load, which also feeds the Pi and is never cut | Validation |
| QHY focuser | Not measured: no rig has one | P0 |

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
  `Connected = true` refused with `0x540`) until the service next starts or
  reloads (for a QHY camera on Windows, until it restarts: decision 2).
  Without a
  list it is not served at all. `zwo-focuser` has no list and serves the
  EAFs the SDK enumerates. A camera that was served, left the bus and came
  back opens again on a plain `Connected = true`, on all three camera
  drivers (measured). Their check for a departed camera is lazy: `Connected`
  and the members served from cache never reach the SDK, so a camera
  switched off while idle reads connected until its next SDK call fails
  (`qhy-camera` C9).
- **The serial drivers run an eager startup handshake** before binding
  their listener: `dsd-fp2`, `ppba-driver`, `upbv2-driver`,
  `gemini-powerbox-adv3-driver`, `qhy-focuser`, `pa-falcon-rotator`,
  `pa-scops-oag` and `star-adventurer-gti`. A port that is absent, or a
  device that does not answer, fails the start, and a reload with the device
  absent ends the process. The units restart on failure every 5 s with no
  start limit, so a boot or an upgrade while a device is off loops until the
  device has power (rig2, 2026-09-06: 608 starts in 69 minutes). Once
  running, the shared transport's supervisor retries a lost link every 5 s
  and never gives up. It warns once when the link is lost, logs each failed
  retry at `debug!`, and logs at `INFO` when the link comes back. The
  transport stays open for the service's lifetime, so it retries whether or
  not a client is connected.
- **rp's reconnect supervisor** retries every configured device every
  `equipment.reconnect_interval` (30 s by default) and never gives up. A
  device lost on a pass gets the connect routine's three attempts, one
  already unreachable on the previous pass gets one, and a placeholder's
  refusal costs one. The loss logs once at `INFO`, each later failed pass at
  `debug!`. A device held out through `disconnect_device` is not tried at
  all. rp sends `Connected = false` only for `disconnect_device`, which
  holds the device out first. Any other disconnect it sees it takes for a
  fault and heals on its next pass: `Connected` is shared by every client
  and does not say who disconnected or why.
- **rp's power map and power tools.** A `switches[]` entry lists its power
  box's outputs, the devices each one feeds, and the ones that are
  protected. `get_switch` and `set_switch` read and switch the outputs, and
  `disconnect_device` and `connect_device` hold a device out of the
  supervisor and give it back (rp.md § Power Switching). `ppba-driver`,
  `upbv2-driver` and `gemini-powerbox-adv3-driver` expose their outputs as
  ASCOM Switch ids, each with an operator label.
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
   An operator starts, restarts or reloads the rusty-photon services, at
   install, upgrade, reboot, by hand, or by saving a setup page (which
   reloads its service), with every device powered, USB and 12 V, and
   devices are switched off only once the services have started. A reload
   looks for its devices again, as a start does. Every device is then found
   at start, and a device switched off later is found again by the fresh
   connect decision 3 makes once its power is back: a camera that was
   served, left the bus and came back opens again on a plain
   `Connected = true` on all three camera drivers, and a serial driver's
   transport retries a lost link every 5 s and never gives up (Where the
   stack stands; P0 confirms it device by device). The Linux and Windows
   packaging guides tell operators so.
   - **On Windows it is the only reliable way for QHY.** The SDK lists a
     camera that was present at the process's first `InitQHYCCDResource`,
     and opens it again by id after any power cycle (7 of 7 on rig2). One
     that arrives later is listed only if the SDK's own arrival listener
     catches it, and on rig2 that listener missed 13 of 30 arrivals. A missed
     camera stayed invisible to the process until its next arrival, and an
     in-process Release + Init did not help (Validation). Under the rule
     every service's first init is the case the measurements cover: made at
     start, with the cameras present.
   - **A device that is off when its service starts or reloads**, after a
     daytime reboot or upgrade, a crash restart or a setup-page save, is
     served as it is today. A listed camera is a placeholder, and an
     unlisted one is not served, until its service restarts (or on Linux
     reloads) with it powered. A serial driver fails its start, or ends its
     process on a reload, and restarts every 5 s until its device answers.
   - **Not a device that binds when it arrives.** The alternative, settled
     on 2026-11-03 and dropped on 2026-11-04, registered every listed camera
     unbound from the start, with its identity stored in its list entry, and
     bound it on a client's connect or on the OS's USB hotplug notification;
     the serial drivers would have served an absent device disconnected
     instead of failing their start. All of it handled a device that is off
     at start, which the rule removes, and for QHY on Windows the binding
     would still have depended on the SDK's arrival listener.

3. **No device's session crosses a power cut** (settled 2026-11-04).
   Before an output is switched off, every device on it is disconnected
   through rp. rp's reconnect supervisor leaves a device disconnected that
   way alone until it is connected through rp again, which the dusk
   document does, or an operator with the same tool, once the output is back
   on, the device has settled and its service has restarted (decision 5).
   A connect through rp never cycles a connection: the restart is what
   makes the session fresh, whatever happened while the device was off,
   including an rp restart that dropped the hold-out (it lives in rp's
   memory) and let the supervisor reconnect a device that stayed on the bus.
   The mount survives a session held across a cut. `star-adventurer-gti`
   reads its controller's init flag in every handshake, writes the mount's
   last position back after a restart, and marks the position lost when the
   mount was slewing (its §Power cut while serving). Other devices'
   sessions held across a cut come back stale:
   - A camera switched off while idle reads connected all day, since the
     departure check is lazy. Once it has returned, its first exposure on
     the stale handle fails `NOT_CONNECTED` and ends the session. Until a
     failure anywhere in its service rescans the bus, a ZWO camera's reads,
     `CCDTemperature` among them, still answer from the SDK's memory
     (`zwo-camera`'s C6). A plain reconnect then works, for a QHY camera
     only once its wheel has been released too (Validation).
   - A QHY178M whose 12 V alone is cut keeps its session, and its wheel
     homes to slot 0 under a cached position: `qhy-camera` answers a settled
     wheel's `Position` from the slot it cached and does not resend the slot
     it last commanded (FW1, FW2), and the wheel drops a command sent while
     it homes.

   A fresh connect seeds the mount from its power-up pose, opens the camera
   afresh, and re-reads every cache its handshake fills (`qhy-camera` C6).
   Its `equipment_changed {connected: true}` is the dusk document's signal
   that a device is ready. The mount's seed is right only if the mount was
   parked at that pose, and the driver seeds only when its mount state
   record says the mount came to rest there. So its park target must be its
   `unpark_from_ap_position` pose. The two match unless someone configures
   them apart (a `SetPark` capture, or another named park), and the dawn
   document checks them before it cuts the mount's power, refusing to cut it
   when they differ. When `star-adventurer-gti` serves through the cut
   (decision 4), it sees the controller restart first and writes the mount's
   last position back, which after a cut longer than 30 s awaits the same
   power-up check. The driver keeps that pending check in its mount state
   record, so the process decision 5 restarts runs it at the dusk connect,
   and the check seeds the pose dawn parked at.
   - **Not rp's supervisor reconnecting.** A disconnect made through
     another client does not stick, because the supervisor heals every
     disconnect, and its reconnect could land while a device is still
     powering up.
   - **Not each driver recovering on its own.** Every driver would need its
     own way to notice a return, and a 12 V-only cut gives it none.
4. **The serial drivers keep their transport running** (settled
   2026-11-04). The transport stays open for the service's lifetime, and
   every driver but `pa-falcon-rotator` polls throughout, so a power cut
   fails the next poll whether the COM port leaves or stays with a dead
   controller behind it. The Falcon polls only when asked, so it notices a
   cut at its next request, when that cut takes its controller. pier1's
   Falcon keeps its controller on USB power and loses only its motor, and
   no request notices that: a move is accepted and reads finished without
   moving (Validation). The supervisor then retries every 5 s and, once
   the device answers, runs the read-only handshake again, which checks the
   device and refreshes the driver's cache. Only the log changes (decision
   6). This
   covers a cut nobody planned; a planned one ends in decision 5's restart,
   which starts every serial driver afresh. A start still fails on a device that does not
   answer: under decision 2 that means a wrong or misconfigured port, and a
   restart loop shows it loudly.
   - **Not retrying only while a client is connected**, nor backing off
     while none is. Either saves the open attempts while a device is off,
     but the dusk connect then waits on the handshake, and a link lost with
     no client attached owes the mount its stop on the next open.
   - **Not opening the port only for a client**, as the transport once did.
     That gives up the check of the device at start and the stop sent with
     no client attached.
5. **Every service whose device was power-cycled restarts at dusk, before
   its devices are connected** (settled 2026-11-04). Once the outputs are on
   and the devices have settled, the dusk document has each such service
   restart: the camera services, the serial drivers, and PHD2. A driver
   restarts itself when rp, as its ordinary client, asks it to. It closes
   its devices and exits with a dedicated code, and the service manager
   starts it again: systemd's `Restart=on-failure` and the SCM failure
   actions it applies to non-crash failures within about 5 s, launchd's
   respawn within about 10 s. PHD2, which is not a rusty-photon service,
   is restarted through `phd2-guider` (decision 10). The document then
   connects the
   devices through rp. Each service then meets its device in a fresh
   process:
   - **A camera.** On rig2 no QHY600M exposure wedged in a fresh process (0
     of 20). Once the camera had been power-cycled while the process's SDK
     was alive, 6 of 30 did, 3 of 6 after an in-process Release + Init, and
     waiting 30 s after the camera returned did not prevent it
     (Validation). A wedged camera reads out for a minute, returns an
     all-zero frame, hangs the close, and needs another power cycle. The
     dev box's QHY178M on Linux wedged in neither case across 22 power
     cycles, but did wedge once in a fresh process, on the first exposure
     after its 12 V had been off for three days with its USB on. The same
     sequence after 40 minutes off did not (Validation), so a fresh process
     makes a wedge rarer without ruling one out.
   - **The guide camera under PHD2.** PHD2 drives it with its own QHY
     driver, whose SDK lives in PHD2's process through the camera's power
     cycle, and PHD2's connections are its own, out of rp's reach.
   - **A serial driver.** A fresh start runs the eager handshake and checks
     the device, the Falcon included, which has no poll to notice the cut,
     and the mount's first connect seeds its encoders.

   The restart also makes decision 2's rule hold every day.
   - **Sentinel stays an independent watcher.** rp asks the driver, and
     `phd2-guider` asks PHD2, never sentinel or the service manager.
     Sentinel restarts a service only after 3 failed health probes 30 s
     apart, so a restart passes under it.
   - **Not a hold before the first exposure.** Waiting 30 s did not help.
   - **Not cycling connections instead.** A disconnect and reconnect through
     rp leaves the SDK's process alive across the cut, and `Connected` is
     shared, so cycling a device another client uses would abort its work.
   - **Not the SDK in a child process.** Frames of up to 120 MB would
     cross a process boundary, and every SDK call would become a message.
   - **Not a workflow with the service manager's rights.** It would make
     session-runner or rp a second supervisor beside sentinel.

6. **A powered-off device is logged once** (settled 2026-11-07). rp logs
   that a device is unreachable at `INFO` once, on the transition, and at
   `debug!` while it stays unreachable, for every device and driver. A
   device that was unreachable on the previous pass gets one attempt, not
   three: rp walks the devices one after another, so 3 s of backoff per
   powered-off device would delay every other device's check. rp knows
   which devices were unreachable from its own record, so the rule needs no
   trust in a driver's error code: any Alpaca driver may use `0x540`, which
   is why the placeholder rule checks the `UniqueID` form. A device
   disconnected through rp (decision 3) is not tried at all. The shared
   transport does the same for its own log: it warns once when a link is
   lost and logs each failed retry at `debug!`. A device off for 16 hours
   otherwise costs about 11,500 warnings.
7. **Dusk starts at civil twilight, once the roof is open** (settled
   2026-11-07). The dusk document starts at a configured offset from civil
   twilight and powers nothing up until rp's safety monitors read safe, with
   the roof open, waiting until dawn at most. On a night the roof stays
   shut, no device is powered at all. The camera then cools in time that
   is already dark.
   - **Not before dark, without the roof.** Nothing in the dusk sequence
     slews the mount, so the document could start early enough for the
     camera to reach its setpoint by dark, but it would power every device
     on nights the roof never opens.
8. **rp's power-box entries map each output to its devices** (settled
   2026-11-07). Each `switches[]` entry lists the power box's outputs, the
   ids of the devices each one feeds, and marks as protected every output
   that feeds the host or its network link (a switch or an access point),
   the power box's own link, or a safety monitor (pier1's Adjustable Output
   feeds its Pi: Validation). A safety monitor stays powered and connected
   because dusk waits for it to read safe (decision 7). P3's
   switch tool refuses to switch off an output while a device on it is
   still connected through rp, and never switches off a protected one.
   Power is rig-wide, so a mount or guide camera that several trains share
   needs no copy.
   - **Not on each device's entry.** The host is no device the roster
     lists, so the power-box entry needs its protected marks anyway, and the
     map would live in two places.
   - **Not on each train.** The host and a shared mount belong to no train.
9. **doctor reads a device's output before it reports the device absent**
   (settled 2026-11-07). For each check a powered-off device fails or warns
   on (`hardware.usb-device`, `hardware.serial-node` for a COM port that
   leaves with its device, `hardware.sdk-devices` for a camera or EAF
   service that enumerates none, `usb-devices.resolve` for a listed camera
   that is off), doctor looks up the device's output in the power map
   (decision 8) and reads it from the power box's driver. A device whose
   output reads off is reported as powered off, not as a fault. One missing
   while its output reads on fails as an absent device does, and so does
   one whose output doctor cannot read. The reading survives an rp restart
   and needs no schedule.
   - **Not rp's hold-out list** (decision 3). It lives in rp's memory, so
     after an rp restart in the day every powered-off device would fail
     again until dusk.
   - **Not a daytime window.** A device that dies during the day would go
     unreported until dusk.
10. **`phd2-guider` starts PHD2 in the operator's session** (settled
    2026-11-07). PHD2 runs as the operator, in their desktop or VNC
    session, and the `phd2-guider` service runs as `rusty-photon` or
    `LocalSystem`, which cannot open a window there. `phd2-guider` stops
    PHD2 through its RPC (guiding stopped, equipment disconnected,
    `shutdown`) and starts it through a launcher in the operator's session,
    which the packaging installs. On Windows the launcher is a Task
    Scheduler task that runs only while the operator is logged on, which
    the service, as `LocalSystem`, may run. On Linux and macOS the service
    account cannot drive the operator's own service manager, so the
    launcher is a small agent started at the operator's login, as a user
    unit or a launch agent, listening on a local socket the service account
    may reach. A PHD2 the RPC leaves running is ended however it was
    started: the agent runs as the operator, and on Windows the service may
    end any process. The dusk document asks for PHD2's restart after the
    driver services its equipment uses (the mount's, which PHD2 reaches
    through Alpaca) have restarted and answer. Once the restarted PHD2
    answers, `phd2-guider` connects its equipment through the RPC, as part
    of the restart the workflow asked for (decision 1): PHD2 starts with
    nothing connected, and the camera facade connects only while PHD2's
    camera is. The start needs
    the operator's session logged on, as PHD2 itself does; without one it
    fails and dusk reports the guider unavailable.
    - **Not `phd2-guider` in the operator's session.** Run at logon instead
      of as a service, it could own PHD2 as its child, but nothing would
      reach it until someone logs on, the service manager would not restart
      it, and packaging, sentinel and doctor would treat it apart from every
      other service.
11. **Dusk checks each camera for a wedge** (settled 2026-11-07). After the
    connect, the dusk document takes one short test exposure per camera. A
    frame that comes late by more than a fixed margin, never comes, or
    reads all zero counts as a wedge: a wedged camera reads out for a
    minute or more, longer than `phd2-guider`'s camera facade waits for a
    frame by default, so through the facade a wedge usually ends with the
    facade giving the frame up. A frame that fails any other way, or cannot
    start (the camera's service or PHD2 not answering), is no wedge, and
    dusk reports the camera unavailable.
    The document then disconnects through rp every device on the camera's
    outputs, the camera and its wheel among them (decision 3), cuts the
    outputs, switches them back on, waits for the devices to settle
    (decision 2), has each of their services restart (decision 5), connects
    them through rp and tries once more. A second wedge gives the camera up
    for the night, with a log line that says so. The guide camera PHD2
    drives takes its test frame through `phd2-guider`'s camera facade,
    which serves that camera to rp, so a rig whose guide camera sits on a
    switched output enables the facade; its retry restarts PHD2 through
    `phd2-guider` (decision 10) where another camera's restarts its
    service. The restart
    makes a wedge rare without ruling one out (decision 5), a wedge that
    slips through costs the camera its night, and the test frame costs
    seconds.

    A wedge can hang the camera's close, so no step waits on it. rp's
    disconnect gives the driver a fixed deadline and holds the device out
    once it passes, whether or not the close returned, and the switch tool
    counts a held-out device as disconnected. A service asked to restart
    gives the close of its devices a fixed deadline and exits with the
    restart code when it passes. Its service manager starts it again as
    after any restart.
    - **Not the restart alone.** The dev box's one wedge in a fresh process
      came after a long outage, the case closest to a day off, and a missed
      wedge shows only as the night's frames coming back all zero.
    - **Not sentinel ending a stuck close.** Its rule, 3 failed health
      probes 30 s apart, takes 90 s or more, and the restart would then
      depend on a hung close failing a probe.

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| P0 | Power-cut survey: for each device on the dev box, pier1 and rig2, disconnect it, cut its power the way the rig does (its 12 V, or its switched USB port), restore it, and connect it again, recording whether it stays on the bus, what stops working, how long it takes to settle, and whether the fresh connect leaves it working. Fills the table above, records whether each camera's first exposure after a power cycle wedges under a live SDK, and runs the dusk sequence itself (power up, settle, restart the service, expose soon after) to check that the restart prevents it | In progress. Measured (Validation): the QHY178M probed on Linux and in a Windows guest, rig2's QHY600M on bare-metal Windows, the dev box's ZWO cameras and QHY178M through dawn and dusk, and every pier1 device, with `dsd-fp2`, `pa-scops-oag` and `star-adventurer-gti` across a cut. Owed (Validation §Owed): rig2's other devices with their drivers across a cut | |
| P1 | rp and the shared transport log a powered-off device once, not every pass or retry (decision 6) | Merged | [#1723](https://github.com/rusty-photon/rusty-photon/pull/1723) |
| P2 | Services restart on request (decision 5): every driver service takes the request on its own API, closes its devices within a fixed deadline (decision 11) and exits with a code the service managers restart on; `phd2-guider` restarts PHD2 through a launcher in the operator's session, which the packaging installs, and connects its equipment (decision 10); rp's tool sends the request | Not started | |
| P3 | rp tools: `disconnect_device` and `connect_device`, where a device disconnected through rp is held out of the reconnect supervisor until it is connected through rp (decision 3), and a disconnect holds the device out once its deadline passes even if the driver's close has not returned (decision 11); `get_switch` and `set_switch`, addressing a power-box output by its name or label, `get_switch` answering with the devices the power map puts on the output, and `set_switch` refusing to switch one off while a device on it is still connected through rp, or ever when the power map marks it protected: an output that feeds the rig's own links (the power box's serial bridge, the hub to the host, the host's network gear), the host itself (pier1's Pi runs off its PPBA's Adjustable Output: Validation), or a safety monitor. The power map lives in rp's `switches[]` entries (decision 8) | In review | `feature/power-cycle-p3-rp-tools` |
| P4 | `session-runner` dusk and dawn documents. Dusk, from an offset from civil twilight once the roof is open (decision 7): outputs on, wait for each device to settle, restart each service whose device was off, PHD2 after the services its equipment uses (decision 10), connect each device through rp, a test exposure per camera, the guide camera through `phd2-guider`'s camera facade, with one power-cycle retry on a wedge (decision 11), `start_cooldown`. Dawn, after a dusk that powered the devices: `start_warmup`, wait for `cooler_warmup_complete`, park and cover, check the mount's park target against its seed pose, disconnect through rp each device on an output it cuts (the safety monitors and the power boxes stay connected: their outputs are protected, decision 8), outputs off | Not started; needs P2 and P3 | |
| P5 | doctor reads a device's output before it reports the device absent (decision 9) | Not started; needs P3's power map | |

P0, P1 and P2 depend on nothing else, P4 needs P2 and P3, and P5 needs
P3's power map.

`gemini-powerbox-adv3-driver` has a start bug of its own: one reply that
does not decode as UTF-8 during its handshake fails the start. It was seen
once, on the dev box's first start on 2026-11-03, the retry 5 s later was
clean, and it is filed as
[#1700](https://github.com/rusty-photon/rusty-photon/issues/1700).

`star-adventurer-gti` kept a pending power-up check only in memory: on
pier1 on 2026-11-07 the restart decision 5 makes took the check with it,
and the restarted process trusted the written-back estimate (Validation).
The driver now keeps the check in its mount state record, as decision 3
needs ([#1722](https://github.com/rusty-photon/rusty-photon/issues/1722)),
and the restart that dusk makes runs it at the dusk connect.

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
| An open handle power-cycled (5 s off), the camera present at the first init | 7 of 7 clean, on both SDK builds, with nothing touching the handle while off or with a call and a close while off. The first call after the return failed at once, the close succeeded, and a reopen by id 3 s or 30 s after the return opened it on the first attempt. The first exposure after one of them wedged (last row, and the 2026-11-05 runs below) |
| `ScanQHYCCD` from a second thread during the 600M's exposures | No effect. 2 exposures of 2 s with 18 scans each through the 1.5 s readout; every scan took under 1 ms and the frames matched the controls |
| The first exposures after a power-up | 3 of the 22 full-frame exposures started 3–9 s after the camera came up read out for 60–66 s and returned an all-zero frame. The close or SDK release that followed hung until the process was killed, and only a power cycle cleared the camera: a new process's first exposure on it wedged the same way. None of the 14 started 30 s or more after power-up wedged; the 2026-11-05 runs below show the delay is not what decides it |

### QHY, Windows bare metal: a reload and the first exposure (2026-11-05)

rig2 again, with the same QHY600M, UPBv2 switching and SDK builds. The probe
gained a mode that uses the camera, closes it, power-cycles it, waits 30 s
after it returns, then releases and initialises the SDK again in process, as
a reload does. It ran as `SYSTEM` with `qhy-camera` stopped. Then
`qhy-camera` itself was started with the 600M on, the camera was
power-cycled, and the service was reloaded with
`sc.exe control rusty-photon-qhy-camera paramchange`.

| Question | Result |
|---|---|
| An in-process Release + Init, the camera present throughout | Listed and opened, 2 of 2, frames good |
| The same after the camera was power-cycled | Listed and opened, 6 of 6, on both SDK builds: the re-init keeps a camera the first init saw. Its first exposure, started about 32 s after the camera came back, wedged in 3 of the 6 |
| `qhy-camera`'s own reload after a power cycle | Camera 0 and its wheel were served as the camera both times, not as placeholders |
| Every full-frame 600M exposure of 2026-11-04 and -05, by what the process's SDK had seen | In a fresh process, with the camera present at the first init: 0 of 20 wedged (the first exposure of the 7 power-cycle runs and the 8 re-init runs, and the 5 of the concurrent-scan run). After the camera was power-cycled while the SDK was alive: 6 of 30, made up of a late arrival opened after the callback 1 of 15, a second arrival 1 of 2, a reopen without re-init 1 of 7, and a reopen after re-init 3 of 6. Not counted: the first exposure of a new process on a camera an earlier run had left wedged, which wedged too, since only a power cycle clears one |

### ZWO and QHY, Linux: dawn and dusk (2026-11-06)

The dev box's cameras on its Gemini Advanced 3 (firmware 3.0.8), Fedora 44
(kernel 7.2.11), ASI SDK 1.41 and QHY SDK 26.06.04, with `zwo-camera`,
`qhy-camera` and `gemini-powerbox-adv3-driver` started with `cargo run` from
`main` at `745de193`. Every switch and every camera call went through the
services' Alpaca APIs. The Gemini's outputs feed: `USB A` the QHY178M,
`USB B` the ASI1600MM-Cool and its built-in USB 2 hub, `USB C` the ASI178MM,
`USB D` the ASI120MC-S, `DC2` the ASI1600MM-Cool's cooler, and `DC3` the
QHY178M's cooler and its wheel. A dawn here is a disconnect, then the
camera's outputs off for 30 s. A dusk is the outputs on, a settle, a restart
of the service or not, a connect, and an exposure.

| Question | Result |
|---|---|
| ZWO dawn and dusk: the three cameras' USB ports and DC2 together | Every camera was back on the bus 0.4–0.6 s after power-on. In 10 cycles, 5 with `zwo-camera` restarted before the connect and 5 with the same process, each camera connected on a plain `Connected = true` 10 s after power-on, and its first exposure was good, 30 of 30 |
| The ASI1600MM-Cool's 12 V cut for 60 s, USB on, its cooler regulating to 8 °C | It stayed on the bus and exposed normally. Nothing reported the cut: `CoolerOn` stayed true and `CoolerPower` climbed from 14 % to 39 % while the sensor warmed from 11.6 °C to 19.0 °C. When the 12 V returned it cooled at once, overshooting to 3.3 °C within 50 s with the power its loop had built up |
| A ZWO session held across a 10 s cut, the cameras idle | Each camera read `Connected == true` once power was back. Its first call failed `NOT_CONNECTED` and ended the session (`zwo-camera`'s C6): an exposure for the first camera, and for the other two a `CCDTemperature` read, after the first camera's check had rescanned the bus. A plain reconnect then exposed normally |
| `CCDTemperature` right after a connect | The ASI1600MM-Cool read 0.0 °C on the first read after every connect, power cut or not, and its real temperature a second later |
| QHY178M dawn and dusk | 22 cycles, all clean. 11 cut `USB A` and DC3 (the camera leaves the bus and is back 1.2–1.5 s after power-on), 6 of them with `qhy-camera` restarted. 11 cut DC3 alone (the camera stays on the bus), 5 of them with a restart. Every connect found the wheel at slot 0, and every first exposure, started 30 s after power-on, was ready 3.3 s after its 1 s start with a good frame |
| QHY178M after three days with DC3 off, its USB on, and no process holding it | The first exposure of a fresh `qhy-camera`, started 30 s after DC3 came on, wedged. It was ready only after 96.7 s, with every pixel 0, and then `CCDTemperature` read −5005.8 °C and the wheel could not report its slot count. Cutting both outputs and restarting the service cleared it. The same sequence after 40 minutes with DC3 off gave a good first exposure |
| A QHY session held across a 10 s cut of both outputs, camera and wheel connected | Both read `Connected == true` once power was back. The camera's first call failed `NOT_CONNECTED`, and then both devices read disconnected. A reconnect of the camera alone was refused until the wheel was disconnected too (`qhy-camera`'s C9). Then both connected and the camera exposed normally |

### pier1 (2026-11-04, 2026-11-06 and 2026-11-07)

pier1 is a Raspberry Pi 5 on Raspberry Pi OS, with a Pegasus PPBA Gen2
switching the rig and the nightly packages of 2026-10-31 installed. The
mount's controller (on a host USB port), the DSD FP2 and the Falcon's motor
share the PPBA's quad 12 V output. The Adjustable Output feeds the Pi and
the SV605CC's cooler. The SV605CC and the QHY5III715C sit on the Pi's USB 3
ports. The PPBA's internal hub carries its own serial bridge on port 1, a
2-port hub with the Scops and the FP2 on port 2, the Falcon on port 3 and
the EAF on port 4. Its `USB Hub` switch powers port 2 alone. On
2026-11-06 the tests ran with rp, sentinel and `star-adventurer-gti`
stopped, so the mount was never commanded. Every other driver kept running,
and each cut went through `ppba-driver`'s Alpaca API. On 2026-11-07 the
nightly packages of that day were installed, with `star-adventurer-gti`'s
restore, and the mount's tests ran with every driver serving. The mount sat
at rest at its `ap_park_3` seed pose (hour angle −6 h, Dec 90°), its state
record said so, and nothing commanded it to move.

| Question | Result |
|---|---|
| The quad output cut for 0.5, 5 and 30 s, with the drivers stopped (2026-11-04) | The mount left the bus about 0.2 s after the cut and was back about 0.55 s after power-on, its controller restarted (`star-adventurer-gti`'s §Power cut while serving). The FP2 left with it and was back about 0.25 s after the mount |
| The Adjustable Output cut for 10 s, sent from the Pi itself (2026-11-06) | The Pi went down with it: the Adjustable Output feeds the host. The PPBA is reachable only through `ppba-driver` on the Pi, so nothing can switch the output back on, and it stays off until someone at the rig restores it |
| The quad output cut for 10 s, every device connected (2026-11-06) | The mount left the bus 0.16 s after the cut and the FP2 0.41 s after, both back 0.5–0.6 s after power-on. `dsd-fp2` logged one failed poll and then a warning at each 5 s retry, read disconnected through the cut, and reconnected on its own, without a restart. Its light then went on and off. The SV605CC kept cooling. The Falcon, the Scops and the EAF answered throughout and then moved and came back |
| The quad output cut for 30 s, the FP2 disconnected first | During the cut the Scops moved and came back. The Falcon accepted a move to 153° with no error, read `IsMoving == false` at once and stayed at 152°, and it did not move when the 12 V returned. Once power was back the FP2 connected and worked on a plain `Connected = true`, and again after its driver was restarted |
| The `USB Hub` switch cut for 10 s, every device connected, then for 30 s with the Scops and the FP2 disconnected first | The PPBA's serial link stayed up. Only the 2-port hub went, with the Scops and the FP2, both off the bus within 0.3 s and back 0.9–1.1 s after power-on. Through the 10 s cut both drivers read disconnected and warned at each 5 s retry, then reconnected on their own. After both cuts each device moved, or lit, on a plain connect, and again after its driver was restarted |
| The quad output cut for 5 s, rp connected to every device (2026-11-07) | The driver logged the controller's restart and wrote the last position back: encoders read `(0, 0)`, written `(−907200, 725760)`. After the cut the mount read the same hour angle and Dec 90°, its record stayed at rest at the seed pose, and the position was not lost |
| The quad output cut for 40 s, rp connected (2026-11-07) | Through the cut the mount read `Connected == false` and its reads failed `NOT_CONNECTED`, and the transport warned at each 5 s retry, eight times. Once the controller answered, the driver wrote the last position back as an estimate (an outage of 43 s), ran the power-up check at once against the record, and seeded `ap_park_3`. The mount read the same hour angle and Dec as before |
| The plan's dawn and dusk by hand, with a 40 s cut (2026-11-07): sentinel and rp stopped, the mount, the FP2 and the Falcon disconnected, the quad cut, their three services restarted, rp started | The driver, serving with no client connected, saw the controller restart once it answered and wrote the last position back as an estimate (an outage of 43 s), leaving its power-up check for the next connect. The restart ended that process, and the check with it. The restarted process found the controller initialised and its encoders at the written-back `(−907200, 725760)`, so rp's connect seeded nothing and checked no record. The mount read the same pose, the one it rested at. The FP2 and the Falcon reconnected on rp's connect |

### Owed

- rig2's other devices.
- pier1's dawn and dusk by hand again, with `star-adventurer-gti` keeping
  its pending power-up check in the mount state record: the restarted
  process should run the check from the record at the dusk connect.
- An output that may feed the host or the power box's own serial bridge is
  not cut remotely at all: on pier1 the PPBA is reachable only through
  `ppba-driver` on the Pi, so once either is down no host can switch the
  output back on.
- Why the QHY178M wedged after three days with only its 12 V off: whether
  the length of the outage decides it.

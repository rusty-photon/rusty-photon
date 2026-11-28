# rig2's mount over Alpaca: ASCOM Remote Server in front of the Astro-Physics driver, 2026-11-27

The native guider's stage G7a
([the plan](../../plans/native-guider.md#staging), decision 13) puts an
Alpaca bridge in front of the vendor's ASCOM driver for rig2's mount,
and gives `rp` on rig2 a mount entry for it.

**The mount answers over Alpaca, and `rp` binds it.** ASCOM Remote
Server serves the Astro-Physics V2 driver as Telescope 0 on
`http://127.0.0.1:11190`. Both it and APCC Pro, through which the
driver reaches the mount, run in the owner's desktop session. That is
the stage's condition as the owner revised it on the night: the bridge
could not be made to run with nobody logged on (§ Session 0, below).

This is the fourth kind of record [the index](../README.md) names: a
check a plan's stage asks for on a rig. Its evidence is the scripts as
run and their output ([mount-snapshots.txt](mount-snapshots.txt),
[alpaca-reads.txt](alpaca-reads.txt)), the driver's log of its connect
([driver-connect.txt](driver-connect.txt), the lines that send a
command or report a state), `rp`'s log and the session-0 attempts'
output ([session-0.txt](session-0.txt)). APCC's own log stays on the
rig: it holds the mount's network address and APCC's licence exchange
with the mount.
No ConformU run was made: its Telescope tests slew the mount, and they
cannot be deselected (see
[hardware-validation.md](../../skills/hardware-validation.md#install-and-run)).

## What was tested

| | |
|---|---|
| Rig | rig2: Windows 11 Pro 25H2 x64 (build 26200), ASCOM Platform 7.1 Update 3 (7.1.3.4851) |
| Mount | Astro-Physics Mach 1 GTO, GTOCP4 controller, firmware `VCP4-P02-17`, on the observatory's Ethernet. Parked at Park 3, pointing at the pole, throughout |
| Mount software | APCC Pro 1.9.8.21 (Astro-Physics Command Center), connected to the mount over TCP, with its REST API on port 60001. The Astro-Physics V2 ASCOM driver 5.60.17 (`AstroPhysicsV2.Telescope`, a 32-bit COM local server), set to reach the mount through APCC's REST API |
| Bridge | ASCOM Remote Server 7.0.1.36689, the 32-bit build the Platform installs (`C:\Program Files\ASCOM\RemoteServer\32bit\RemoteServer.exe`) |
| `rp` | The nightly suite MSI `0.1.0+nightly.202611071139.g21bfe94`, installed on 2026-11-07, as in the [guide camera's record](../2026-11-17-qhy-camera-qhy5iii678m-windows/README.md) |
| Time | 2026-11-28 03:40 to 04:45 UTC (21:40 to 22:45 at the rig, 2026-11-27), the roof open; and 13:29 to 16:21 UTC on 2026-11-28 for § The morning after, the roof closed |

## How the rig is set up

**Remote Server's settings**, which it keeps in the registry of the
account it runs as (`HKCU\Software\ASCOM Remote`). The rest keep
Remote Server's defaults.

| Setting | Value | Why |
|---|---|---|
| Server IP Address, Port Number | `127.0.0.1`, `11190` | `rp` and the guider run on the same host. Its default port, 11111, is `filemonitor`'s, and 11190 is outside the ports the workspace hands out |
| Server Auto Connect, Start With API Enabled | `True`, `True` (the defaults) | Remote Server serves nothing until it has created and connected its drivers, and this does both as it starts |
| Management Interface Enabled | `True` | `rp` and the guider read `GET /management/v1/configureddevices` on every connect |
| Alpaca Discovery Enabled | `False` | No client of the bridge finds its devices by discovery |
| Check For Updates, Check For Pre-release Updates | `False`, `False` | No one is there to answer an update prompt |
| Server Log Folder, Maximum Retained Files, Maximum File Size | The account's `%LOCALAPPDATA%\ASCOM Remote\Logs`, `5`, `50` MB | Meant to keep the logs out of the account's synced Documents folder and bound them. 7.0.1 keeps neither: it has no retention settings, which came to Remote Server's source after it, and it writes to this folder only until its first midnight (§ The morning after) |
| Served device 0 | Telescope, `AstroPhysicsV2.Telescope`, device number 0, description "Astro-Physics Mach 1 GTO" | Remote Server minted its UniqueID, `7159159E-BEFF-4370-A997-AF8F43FBCC45`, on its first start |
| Allow Connected Set False (device 0) | `False` | A client that disconnects does not disconnect the driver, so `rp` going away cannot take the mount from the guider. Remote Server answers it and leaves the driver connected |

**URL reservations.** Remote Server runs unelevated, so http.sys takes
its four prefixes only with a reservation for the owner's account:
`http://127.0.0.1:11190/api/`, `/server/`, `/management/` and `/setup/`
(`netsh http add urlacl`). Without them it would try to run ASCOM's
SetNetworkPermissions elevated, which prompts.

**One scheduled task**, "Mount bridge at sign-in (started for
rusty-photon)", runs in the owner's session with the owner's
interactive token (not elevated). Its trigger, 30 s after the owner
signs in, is disabled until the driver's connect on an unparked mount
that is not tracking has been watched (§ What this leaves), so the
task is run by hand when the bridge is wanted. It runs
[start-mount-bridge.ps1](start-mount-bridge.ps1), kept in the account's
`%LOCALAPPDATA%\ASCOM Remote`, which starts the two programs in order:

1. APCC Pro, which connects to the mount when it starts (its
   AutoConnect setting);
2. once APCC's REST API accepts a connection on `127.0.0.1:60001`,
   where the driver reaches it, Remote Server.

The order matters because Remote Server connects the driver as it
starts, and the driver gives up on an APCC that has not answered
within its connect timeout, 15 s in its settings. APCC took 5 s from
its start to its REST API at 21:41 and 14 s at 22:43. The script waits
10 minutes at most, starts nothing more after that, and writes each
step to `mount-bridge-start.log` in Remote Server's log folder.

Remote Server is started as its 32-bit build: the 64-bit build reads
its `RunAs64Bit` setting, `false`, and starts the 32-bit build in its
place.

**`rp`'s mount entry**, added to `rp.json`'s `equipment` block:

```json
"mount": {
  "alpaca_url": "http://127.0.0.1:11190",
  "device_number": 0,
  "unique_id": "7159159E-BEFF-4370-A997-AF8F43FBCC45"
}
```

It has no `guiding` block yet; stage G7 adds one.

## What was run, and what it showed

1. **The mount before.** [mount-snapshot.ps1](mount-snapshot.ps1) sends
   read-only LX200 get commands to the mount's own web interface, the
   ones APCC polls ([mount-snapshots.txt](mount-snapshots.txt)). Before
   anything started, the mount read Dec +89°59′58.9″, its status string
   starting `P`, and its right ascension advanced with the clock:
   parked, not tracking. Its clock was 4.9 s off.
2. **APCC.** APCC started three times on the night: in session 0 at
   21:41 (§ Session 0), in the owner's session at 21:58 by hand, and
   at 22:43 by the sign-in task, from a stopped chain. Each time it
   connected to the mount, and the first set the mount's date and time.
   Its logs of all three show it polling the mount's status, setting
   the guide rate (`:RG2#`), sending a few other short commands once as
   it connected and `:SPC` every few seconds after. Around the first
   and the third ([mount-snapshots.txt](mount-snapshots.txt)), the
   mount's Dec, pier side and status read as before, and its right
   ascension still advanced with the clock. Its reported azimuth
   changed once, from 0°02′39″ to 0°00′09″, after APCC's first
   connect, and not after the third. Its Dec held it 1.1″ from the
   pole throughout, where the azimuth of where it points can change by
   a few arcseconds at most, so that change is in what the mount
   computes, not where it points.
3. **The driver's connect.** Remote Server created the driver and set
   `Connected = True` as it started, at 21:59 and again at 22:43, when
   the sign-in task's ordered start left the mount as it was
   ([mount-snapshots.txt](mount-snapshots.txt)). On the parked mount the driver
   sent the long-format toggle (`:U#`), the guide rate 1.0× (`:RG2#`),
   a time sync to APCC (`:*TIMESYNC,1#`) and `:B1#`, and otherwise only
   status reads ([driver-connect.txt](driver-connect.txt)). It logged
   "Completed move to parked position" without sending a move. The
   mount stayed parked. Its log of 2026-11-22, a connect with the mount
   unparked and tracking, shows it sending a tracking rate (`:RT3#`)
   while logging "Restoring tracking state = 1", with the mount tracking
   before and after. A connect with the mount unparked and not tracking
   has not been observed: the logs do not show whether the state the
   driver restores is the mount's own or one the driver kept, and the
   driver's release notes for 5.60.16 say a tracking state it restores
   can unpark an AP mount. Remote Server connects the driver each time
   it starts, so § What this leaves holds that case for the owner, and
   the task's sign-in trigger is disabled until it has been watched.
4. **The mount over Alpaca.** [alpaca-read.ps1](alpaca-read.ps1) read
   the server's device list and 30 properties of Telescope 0, every one
   a `GET` ([alpaca-reads.txt](alpaca-reads.txt)): `Connected` true,
   `AtPark` true, `Tracking` false, `Slewing` false, `CanPulseGuide`
   true, both guide rates 0.004178°/s (15.04″/s, 1.0× sidereal),
   `SideOfPier` 1 (pierWest), `InterfaceVersion` 2. The reads were made
   at 04:04:23 UTC.
5. **`rp`.** Restarted with its mount entry, `rp` bound it, the pin
   matching ([rp-binding.txt](rp-binding.txt)). It logged no site
   mismatch, for which it refuses a mount's connect: the mount's site
   agrees with `rp`'s within 0.01°. The only command the driver
   logged from it is `SET Connected = True`, which Remote Server passed
   to a driver already connected. The mount stayed parked.

## Session 0

The stage first asked for the mount to answer with nobody logged on.
The chain was run as scheduled tasks under the owner's account, by
S4U, in session 0, and it failed at the bridge:

| Piece | In session 0 |
|---|---|
| APCC Pro | Ran, connected to the mount and served its REST API |
| The driver | Ran when started by a full administrator's token, connected through APCC, and left the parked mount parked |
| Remote Server, the task's own token | An S4U task's token is a full administrator's whatever its run level (`whoami /groups` from such a task: Administrators enabled, High integrity). Remote Server created and connected the driver and started its listener, then warned that it runs as an administrator with a message box. In session 0 that throws, and Remote Server's handler for it throws in turn, which ended the process |
| Remote Server, a basic-user token (`runas /trustlevel:0x20000`) | Administrators deny-only, so no warning. But the driver's COM server would not start for it: Windows' default launch permission names SYSTEM, Administrators and INTERACTIVE, and a batch logon has none of them (DCOM event 10016). With a launch permission for the owner added to the driver's AppID, COM started the driver's process, but the driver never created the object Remote Server asked for: its log stops where it would, and Remote Server's request had not returned 2 minutes later, when both were stopped |

The launch permission, the S4U tasks and the session-0 APCC were
removed afterwards; the URL reservations stay, since the desktop
session needs them too.

## The morning after

At 07:29:04 on 2026-11-28 the roof file read closed, and `rp` parked the
mount through the bridge: Remote Server's trace shows its `PUT
/api/v1/telescope/0/park` answered with no error
([remote-server-logs.txt](remote-server-logs.txt)). At 10:09:17 `rp`,
started on the nightly `0.1.0+nightly.202611281404.g3e1700e` after its
install with the roof still closed, parked it again, also answered with
no error. Both were the first parks `rp` made of rig2's mount, of a mount
already parked. It read parked before and after
([mount-snapshots.txt](mount-snapshots.txt)): status `P`, `AtPark` true
at 10:09:31. Its Dec read 89°59′59.0″ after both, 0.1″ from the night's
last reading, and the park at 07:29 is the only command on record
between the two. `rp`'s own log shows
neither park: it logs a safety park only at debug level (issue
[#1947](https://github.com/rusty-photon/rusty-photon/issues/1947)).

The trace that shows them was not in Remote Server's configured log
folder. At its first midnight, 00:00:16, Remote Server 7.0.1 closed its
log there and opened the new day's trace and access log under the
account's `Documents\ASCOM\Logs 2026-11-28`, which on rig2 is in OneDrive.
Its source at the 7.0.1 tag builds the new logger with a constructor
that marks its folder as automatic before setting the configured
folder, and the mark wins; its current source passes the folder to the
constructor. By 10:16 the two files held 1.8 MB, about 4 MB a day with
`rp` polling the mount, and 7.0.1 bounds neither their size nor their
number.

## What this leaves

- **The bridge needs the owner's session signed in.** Disconnected is
  enough. After a sign-out, or a restart that nobody signs in after,
  the mount is off Alpaca until the owner signs in and the task is run.
  Windows did not sign the owner in after the restart of 2026-11-26:
  the owner's session then began with a Remote Desktop logon 48 s after
  the boot.
- **The driver's connect on an unparked mount that is not tracking is
  not observed**, and it is held for the owner. Remote Server makes
  that connect each time it starts, whatever the mount is doing, so if
  the driver restores a tracking state it kept, a start of the bridge
  could start the mount tracking or unpark it (project tenet 3). The
  owner chose on 2026-11-27 to keep the task's sign-in trigger disabled
  until one such connect has been watched, which means leaving the
  mount unparked with its tracking off first. Until then the task is
  run by hand, in the owner's session, with the mount parked or
  tracking.
- **The sign-in trigger has never fired.** The task's action was run by
  hand in the owner's signed-in session, from a stopped chain: APCC at
  22:43:39, its REST API accepting at 22:43:53, Remote Server and the
  driver at 22:43:53, the driver connected at 22:43:55.
- **A mount that is off or unreachable when the task runs leaves the
  bridge down.** The driver's one connect gives up after its 15 s
  timeout, and Remote Server then reports the mount unavailable until
  Remote Server is stopped and the task is run again (the script starts
  no second copy). The owner chose on 2026-11-27 to leave it so: the
  Mach 1 has stayed powered since 2026-11-22.
- **Remote Server's logs grow in the synced Documents folder**, a file of
  each kind a day at about 4 MB a day, from each start's first midnight
  on, and nothing removes them. A Remote Server built from its current
  source, which has the retention settings and the rollover fix, would
  bound them; the newest tag of its repository is 7.0.1. Turning its
  trace and access log off stops them, at the cost of the record they
  keep.
- **APCC in the session means APCC by hand stays as it was**: its
  window is the owner's to use, and a second copy must not be opened.
- **While the owner's session is signed in, rig2's mount is `rp`'s to
  park.** When the roof file reads closed, `rp` parks the mount through
  the bridge, and APCC parks it at Park 3, which the owner confirmed is
  roof-safe. With the session gone, the bridge and APCC are gone too:
  `rp`'s park finds no mount and fails, and nothing of the workspace
  parks it before the roof closes.

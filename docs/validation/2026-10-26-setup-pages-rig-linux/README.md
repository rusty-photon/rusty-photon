# Setup pages on the field rig, Linux (driver-setup-pages, rig validation)

The rig check the driver-setup-pages plan's §Testing asks of `pier1`
([archived plan](../../plans/archive/driver-setup-pages.md#testing)): each
service's setup pages opened from another machine, a change saved on each
(on `ui-htmx`'s, a save that changed nothing) and seen through its reload
or restart, and a credential set on a driver's
page and entered on `rp`'s. Every page served, and every save landed: the
reloading services reopened their page by themselves within 1.0–1.8 s
showing the saved value, and the restarting ones named the waiting fields
until Sentinel's Restart button restarted them. Each change was then saved
back to the value it replaced, so the configuration ended as it began; the
credentials ended rotated.

This is not a ConformU run: no driver's Alpaca behaviour was under test.
`rig2`'s record (Windows, its FocusLynx entry) is still owed.

## What was tested

| | |
|---|---|
| Commit | [`355ae19b`](https://github.com/rusty-photon/rusty-photon/commit/355ae19b) (`main`, the merge of #1647), packaged by the nightly run [40429133285](https://github.com/rusty-photon/rusty-photon/actions/runs/40429133285) as `0.1.0+nightly.202610270052.g355ae19` |
| Build | The run's `packages-arm64` artifact: the debs its `linux / arm64` leg built and verified. The run published nothing, because its Windows MSI leg failed the upgrade proof on zwo-camera's renamed `max_adu_reporting` values (#1648), so the debs were installed from the artifact with `apt-get install ./<deb>…`, not from the package repository |
| Host | the field rig (Raspberry Pi 5, Raspberry Pi OS, Debian 13, aarch64). Every service serves TLS (the Let's Encrypt wildcard doctor manages) and asks for the observatory credential |
| Upgrade | From nightly `g7bc4e72` (2026-10-08), all 18 installed packages, `rp` stopped first. Before it, each new binary's read-only `doctor --config` passed `config.full-shape` on the rig's 17 configs, and the new central doctor found nothing new, so no file was migrated; the upgrade itself changed no config file. Central doctor warned of ui-htmx's retired `sentinel` block, which ui-htmx's first page save dropped (below) |
| Client | A Fedora 44 x86_64 machine on the rig's LAN, standing in for the laptop: headless Firefox 158.0 driven over W3C WebDriver (geckodriver 0.37.1). Each page was opened, filled and saved through the browser, so the requests carried the browser's own `Sec-Fetch-Site` and `Origin`, and the reload wait ran its own script. The Basic credential was entered once per origin |

## Results

Opening every page, and the save each service took. Each save was followed
by a second that restored the value; both are counted in the times.

| Service | Pages opened | Saved | What followed |
|---------|--------------|-------|---------------|
| `dsd-fp2` | `/setup`, cover calibrator 0 | `cover_calibrator.description` | Reloaded; the page reopened by itself in 1.0 s and 1.3 s with the saved value, and the device's Alpaca `Description` returned it |
| `ppba-driver` | `/setup`, switch 0, observing conditions 0 | `switch.description` | Reloaded, 1.3 s each; `Description` followed |
| `pa-falcon-rotator` | `/setup`, rotator 0, switch 0 | `rotator.description` | Reloaded, 1.3 s and 1.0 s |
| `pa-scops-oag` | `/setup`, focuser 0 | `focuser.description` | Reloaded, 1.0 s each |
| `filemonitor` | `/setup`, safety monitor 0 | `device.description` | Reloaded, 1.3 s and 1.0 s; see observation 1 |
| `star-adventurer-gti` | `/setup` (the USB transport chosen, the UDP variant's fieldset disabled), telescope 0, its recovery page | `mount.description` | Reloaded, 1.3 s and 1.0 s. Before and after, `AtPark` true, `Tracking` and `Slewing` false: the reload moved nothing. No recovery command was sent |
| `svbony-camera` | `/setup` (Cameras section, no list), camera 0 | the camera's override `description`, then cleared | Reloaded, 1.3 s and 1.0 s. The `devices` map gained the camera's entry under its serial and lost it again when the field was cleared; `Description` followed both. `CoolerOn` stayed false |
| `qhy-camera` | `/setup` (Cameras section with its wheel column), camera 0 | the camera's override `description`, then cleared | Reloaded, 1.8 s and 1.0 s; as for `svbony-camera` |
| `zwo-camera` | `/setup` (no ZWO camera on the rig: "This server serves no device") | `max_adu_reporting` to `zwo_driver`, then back to `sensor_ceiling` | Reloaded, 1.3 s each; the dropdown showed both labels and the field's hint, and the file followed |
| `zwo-focuser` | `/setup`, the EAF's page | the EAF's override `description`, then cleared | Reloaded, 1.0 s each; `Description` followed, and the cleared override was deleted |
| `phd2-guider` | `/setup` | `stop_timeout`, 10 s to 11 s and back | Restart-on-save: the saved page named `stop_timeout` and how to restart, and `/setup` kept naming it, until the unit was restarted from Sentinel's dashboard (its Restart button, confirmed, a new PID, the row reading "Restarted, healthy again"); then it named nothing and showed 11 s. The same again for the way back |
| `ui-htmx` | `/setup` | nothing changed (its editable settings are `rp`'s address and CA, which carry the `rp` credential, and its own listener) | "Saved. Nothing changed, so no reload is needed." The save rewrote the file without the retired `sentinel` block, kept `rp.auth`, and kept the file's owner and mode |
| `rp` | `/setup`: every section, the plugins note, the roster of 11 entries with each driver's server and device page linked | the cover calibrator's credential (below) | Restart-on-save, as `phd2-guider` |

**The credential.** On `dsd-fp2`'s server page, the Security section took
a new password and its repeat. After the reload the old credential got
`401` and the new one `200`. The page's reload wait reopened the page into
that `401`, which a browser answers with its login prompt; logged in again,
the page served. `rp`'s roster row for the panel now showed its LED off.
Typed into that row's password field, the new password was saved: the page
named `equipment.cover_calibrators` as waiting for `rp`'s restart, and the
file held the new password. Restarted from Sentinel, `rp` connected the
panel with it, and all 11 roster entries read connected. The row's LED was
on and the page named nothing waiting.

## Not covered

- `qhy-focuser` (its unit is disabled and its device is not on the bus)
  and `sky-survey-camera` (config-gated, with no config) served no page.
  `calibrator-flats` and `plate-solver` have no setup pages, and
  `planetarium-bridge`, `session-runner`, `focus-model`, `polar-align` and
  `upbv2-driver` are not installed on this rig.
- The cameras' list mode and the confirmation a renumbering asks for: the
  rig's camera numbers, which `rp`'s entries are bound to, were left as
  they were.
- The bound-credential confirmation (a changed address with its stored
  credential kept or removed), and the GTi's recovery commands.

## Observations

1. **A reload drops a driver's clients, and a safety monitor's makes `rp`
   park the mount.** After its reload a device reads `Connected = false`
   until its client reconnects. `rp`'s reconnect supervisor did, within
   its 30 s interval, for every driver here. `rp` reads a disconnected
   safety monitor as unsafe. The two `filemonitor` saves, the change and
   the restore about 3 s apart, set off one unsafe transition (`safety
   monitor read failed; treating as unsafe`, then `conditions unsafe`, at
   01:24:18 UTC). The gate stayed closed for 30 s, until the reconnect
   and the next safety poll ("conditions safe again", 01:24:48). No
   session was running, so nothing was cancelled, and the transition
   parked the mount: `rp` sent the GTi a `Park`, and the GTi sent a
   goto (`:G`, `:S`, `:J` on both axes) to its park target. The mount was
   already at that target (the `:j` position reads before and after are
   identical), so nothing moved. Anywhere else, the save would have
   parked it. The setup crate's
   [§ Reload](../../crates/rusty-photon-setup.md#reload) and
   [filemonitor.md § Setup pages](../../services/filemonitor.md#setup-pages)
   now say so. The park is meant: `rp` parks
   on a monitor it cannot read, whatever is running
   ([#1468](https://github.com/rusty-photon/rusty-photon/issues/1468), closed as by design). The reload does not announce
   itself, so `rp` cannot tell it from a monitor that is gone;
   [#1652](https://github.com/rusty-photon/rusty-photon/issues/1652) has the monitor announce it with a deadline `rp` waits for.
2. **`rp`'s roster summary shows less than `rp.md` says.** The collapsed
   row shows the live state as an LED; the bound device's name and
   `UniqueID` are in the opened row ("Bound to Deep Sky Dad FP2 · …").
   [rp.md § The roster](../../services/rp.md#the-roster) puts them in the
   summary.
3. **`rp`'s package starts `rp` during the upgrade**, so stopping it
   first did not keep it from seeing the drivers restart after it; its
   reconnect supervisor reconnected every one, `filemonitor` included,
   without a restart.
4. **The credentials were rotated afterwards**, since the run's operator
   session had exposed the old one. `doctor auth rotate` stopped at
   `sky-survey-camera.json`, an installed service with no config, and left
   the services after it on the old hash (#1649 fixes it). The rest was
   finished by hand, the way rotate would have. The part rotate did run
   rewrote `rp`'s per-target credentials, the panel's temporary one
   included, which #1647 added.

## Evidence

`pages.txt` is the outline of every page above, read from the browser's
DOM after the run by opening each page only, nothing saved: its title,
banners, sections, fields (a password field only as empty or filled),
file-only notes, hints, errors and notes, each collapsed row's summary
with its LED's state and the opened row's details, links with their
labels, and buttons. The text is the DOM's, untruncated, including what a
closed `<details>` hides (`rp`'s roster rows, whose LED and "State" /
"Bound to" lines back observation 2). The one edit made to it: each of
the rig's hostnames is replaced by `<rig-host>`, so every line that edit
touched contains `<rig-host>`.

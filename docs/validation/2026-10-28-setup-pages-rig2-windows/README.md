# Setup pages on rig2, Windows (driver-setup-pages, rig validation)

The rig check the driver-setup-pages plan's §Testing asks of `rig2`
([archived plan](../../plans/archive/driver-setup-pages.md#testing)), the
Windows rig whose `rp` roster names Optec's FocusLynx server: each
service's setup pages opened from another machine, a change saved on each
(on `ui-htmx`'s, a save that changed nothing) and seen through its reload
or restart. Every page that rig2 serves loaded, and every save landed:

- `filemonitor` and `qhy-camera` reloaded in process and reopened their
  page by themselves within 1.3–6.3 s, showing the saved value;
- `rp` named the waiting field until Sentinel's Restart button restarted
  it;
- `rp`'s FocusLynx row linked Optec's own pages.

Each change was then saved back to the value it replaced, and every
config file ended byte-identical to its copy taken before the upgrade.
Two services served no page: `dsd-fp2`'s panel was unpowered, and
`upbv2-driver` stays stopped on this rig. So the credential round trip is
`pier1`'s alone ([its record](../2026-10-26-setup-pages-rig-linux/README.md)).

This is not a ConformU run: no driver's Alpaca behaviour was under test.

## What was tested

| | |
|---|---|
| Commit | [`63979d2f`](https://github.com/rusty-photon/rusty-photon/commit/63979d2f) (`main`, the merge of #1655), published by the nightly run [40607323460](https://github.com/rusty-photon/rusty-photon/actions/runs/40607323460) as `0.1.0+nightly.202610280013.g63979d2` |
| Build | The published nightly's suite MSI, `rusty-photon-0.1.0+nightly.202610280013.g63979d2-x64.msi`. Its SHA256 matched the release's `SHA256SUMS.txt`; nightlies are not Authenticode-signed |
| Host | `rig2`: Windows 11 Pro (10.0.26200, x64) at a remote observatory, the services running under the SCM. Every service serves TLS with a certificate from doctor's private CA and asks for the observatory credential. Pegasus Unity owns the UPBv2 hub's port, so `upbv2-driver` is kept stopped. Optec's FocusLynx Alpaca server (plain HTTP, port 4247) drives the focuser. The cameras and the flat panel were unpowered |
| Upgrade | From nightly `g689f851` (2026-10-03) with `msiexec /i … /qn`, in place, the feature set carried over (Programs & Features: `0.1.0.26276` to `0.1.0.26301`). Before it, each new binary's read-only `doctor --config`, run from an administrative extract of the MSI, passed on rig2's 7 configs. The new central doctor reported the same 4 hardware findings as the installed one: the flat panel's and the QHY cameras' devices absent, and one USB device Windows had stopped (code 43). No file was migrated, and the upgrade changed no config file |
| Client | A Fedora 44 x86_64 machine off-site, standing in for the laptop: headless Firefox 158.0 driven over W3C WebDriver (geckodriver 0.37.1). It reached rig2 over SSH, with each service's port forwarded to the client's loopback, because rig2's certificates name only `localhost` and the rig's own host name. The browser accepted those certificates for the session, and `curl` verified the same endpoints against rig2's CA. Each page was opened, filled and saved through the browser, so the requests carried the browser's own `Sec-Fetch-Site` and `Origin`, and the reload wait ran its own script. The Basic credential was entered once per origin |

## Results

Opening every page, and the save each service took. Each save was followed
by a second that restored the value; both are counted in the times.

| Service | Pages opened | Saved | What followed |
|---------|--------------|-------|---------------|
| `filemonitor` | `/setup`, safety monitor 0 | `file.polling_interval`, 1m to 2m and back; `device.description` | Reloaded in process, with the same PID throughout. The server page reopened by itself in 1.3 s both times and the device page in 1.3 s and 1.5 s, each with the saved value. The device's Alpaca `Description` followed. See observation 1 |
| `qhy-camera` | `/setup` (Cameras section in auto mode; with no camera powered there is no device page) | `server.discovery_port` set to 32227, then cleared | Reloaded in process (same PID), and the page reopened in 6.3 s both times. The file gained the key and lost it again |
| `ui-htmx` | `/setup` | nothing changed (its editable settings are `rp`'s address and CA, which carry the `rp` credential, and its own listener) | "Saved. Nothing changed, so no reload is needed." The file was rewritten byte-identical |
| `rp` | `/setup`: every section, the plugins note, and the roster of 4 entries with each driver's server and device page linked | `safety.poll_interval`, 10s to 15s and back | Restart-on-save. The saved page named `safety.poll_interval` as waiting, until Sentinel's dashboard restarted the service: its Restart button, confirmed; a new PID within a second; the row reading "Restarted, healthy again". `rp` then logged `safety monitoring started … interval=15s`, and the page named nothing and showed 15s. The same again for the way back |
| FocusLynx, in `rp`'s roster | the `focuslynx` row: `http://127.0.0.1:4247 · device 0`, no credential, its light off (the focuser's port was absent) | — | The row linked `http://localhost:4247/setup` and `…/setup/v1/focuser/0/setup`, its loopback host replaced by the one the browser used. On rig2 both answer `200` with Optec's "FocusLynx Alpaca" pages |

## Not covered

- `dsd-fp2` and `upbv2-driver` served no page (observations 2 and 3).
  So the credential set on a driver's page and entered on `rp`'s was
  not repeated here, and `pier1`'s record is the one that covers it.
- `qhy-camera`'s device pages, the cameras' list mode and the renumbering
  confirmation: no camera was powered.
- FocusLynx's pages were checked from rig2, not opened in the browser:
  port 4247 was not forwarded.
- The bound-credential confirmation (a changed address with its stored
  credential kept or removed).

## Observations

1. **A `filemonitor` save keeps `rp`'s session on Windows too.** During
   the two server-page saves, a client read `is_safe` every 200 ms through
   the tunnel: 186 of 186 reads returned `true`, the slowest in 384 ms.
   `rp`, polling every 10 s, logged no warning and no safety transition.
   On `pier1` on 2026-10-26, before #1655, the same saves made `rp` read
   the monitor unsafe (that record's observation 1).
2. **The MSI starts every automatic service, `upbv2-driver` included.**
   On rig2 Pegasus Unity holds that driver's port, so it fails at startup
   ("Access is denied"), and the SCM restarts it every 5 s. After the
   2026-10-03 upgrade it went on retrying for days. A plain stop lands
   between restarts and does not end the loop. It was ended by disabling
   the service until its next restart failed, then setting it back to
   automatic, which leaves it stopped as it was found.
3. **`dsd-fp2` without its panel restarts every 5 s.** The upgrade
   restarted it while the panel was unpowered, so its startup check found
   no port and the service stopped with a failure, which the SCM restarts
   after 5 s. That is the contract for a serial driver whose device is
   absent
   ([service-lifecycle crate](../../crates/rusty-photon-service-lifecycle.md)).
   It serves again once the panel has power. The process the upgrade
   replaced had started while the port existed, and had been retrying it
   in process since.
4. **`rp`'s roster rows** show the light, id, name, address and
   credential in their summary, and the state in words with "Bound to"
   in the opened row ("Bound to: File Safety Monitor · filemonitor-001"),
   as on `pier1`.

## Evidence

`pages.txt` is the outline of every page above, read from the browser's
DOM after the upgrade and before the saves, by opening each page only:
its title, banners, sections, fields (a password field only as empty or
filled), file-only notes, hints, errors and notes, each collapsed row's
summary with its light's state and the opened row's details, links with
their labels, and buttons. The text is the DOM's, untruncated, including
what a closed `<details>` hides. Its headings name each service and the
loopback port the client reached it on. The one edit made to it: the
roof status file's network share is replaced by `<share-host>` and
`<share>`.

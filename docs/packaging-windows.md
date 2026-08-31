# Windows packaging & deployment guide

How to install, configure, and operate the rusty-photon Windows suite
installer on an imaging box. Architecture decisions live in
[ADR-015](decisions/015-windows-packaging-architecture.md) (one MSI, service
model, LocalSystem, config/log locations) and
[ADR-013](decisions/013-native-sdk-payload-policy.md) /
[ADR-014](decisions/014-zwo-per-device-services-and-link-features.md)
(native camera-SDK
payloads); the full design in
[docs/plans/windows-packaging.md](plans/windows-packaging.md); the WiX
source contract in [installer/README.md](../installer/README.md). The Linux
guide is [docs/packaging.md](packaging.md).

## What gets installed

One `rusty-photon-<version>-x64.msi` for the whole family, downloaded from
the GitHub Releases page. The installer presents a feature tree:

- **Core** (required): `sentinel` (watchdog/notifications) and `ui-htmx`
  (web config UI). Any install includes them. Core also installs
  `rusty-photon-doctor.exe` (diagnosis, `--fix` repair, and the TLS +
  credential lifecycle — [docs/services/doctor.md](services/doctor.md);
  there is no separate doctor package) and registers the Scheduled Task
  `rusty-photon-renew` (daily, 03:00, LocalSystem) running
  `rusty-photon-doctor.exe tls renew`; uninstall removes the task.
- **Drivers** (optional, off by default): one sub-feature per device
  driver.
- **Automation** (optional): `rp`, `session-runner`, `plate-solver`,
  `phd2-guider`, `calibrator-flats`, `polar-align`.

Every selected service installs
`%ProgramFiles%\rusty-photon\rusty-photon-<svc>.exe` and registers a
Windows service named `rusty-photon-<svc>` (LocalSystem; auto-start,
except the config-gated five, which install as *Manual* — see below)
with restart-after-5s failure actions — the systemd
`Restart=on-failure`/`RestartSec=5` parity the serial drivers' eager
hardware validation depends on — plus an inbound firewall exception on
its port:

| Service | Port | Feature ID | Notes |
|---------|------|------------|-------|
| filemonitor | 11111 | `Filemonitor` | Alpaca SafetyMonitor |
| ppba-driver | 11112 | `PpbaDriver` | serial (COM port) |
| qhy-focuser | 11113 | `QhyFocuser` | serial (COM port) |
| sentinel | 11114 | `Core` | dashboard: `/` |
| rp | 11115 | `Rp` | orchestrator API |
| sky-survey-camera | 11116 | `SkySurveyCamera` | config-gated (see below) |
| star-adventurer-gti | 11117 | `StarAdventurerGti` | serial (COM port) |
| pa-falcon-rotator | 11118 | `PaFalconRotator` | serial (COM port) |
| dsd-fp2 | 11119 | `DsdFp2` | serial (COM port) |
| ui-htmx | 11120 | `Core` | web config UI |
| qhy-camera | 11121 | `QhyCamera` | needs QHY's All-in-One pack (below) |
| zwo-camera | 11122 | `ZwoCamera` | its SDK DLL bundled |
| pa-scops-oag | 11123 | `PaScopsOag` | serial (COM port) |
| zwo-focuser | 11124 | `ZwoFocuser` | its SDK DLL bundled |
| phd2-guider | 11130 | `Phd2Guider` | wraps PHD2 (installed separately) |
| plate-solver | 11131 | `PlateSolver` | config-gated; needs ASTAP (below) |
| calibrator-flats | 11170 | `CalibratorFlats` | config-gated |
| session-runner | 11171 | `SessionRunner` | config-gated |
| polar-align | 11172 | `PolarAlign` | config-gated |

Alpaca UDP discovery is deliberately not served (as on Linux): point
clients (N.I.N.A. etc.) at `host:port` directly using the table above.

## Installing

Run the MSI and pick features in the tree, or install silently:

```text
msiexec /qn /i rusty-photon-<version>-x64.msi ADDLOCAL=ALL
msiexec /qn /i rusty-photon-<version>-x64.msi ADDLOCAL=Core,ZwoCamera,ZwoFocuser
msiexec /qn /i rusty-photon-<version>-x64.msi ADDLOCAL=Core,Drivers,Automation
```

Feature IDs are the table above plus the group features `Drivers` and
`Automation` (selecting a group selects all its children). `Core` is
always installed. Verify with:

```powershell
Get-Service rusty-photon-*
curl.exe http://localhost:<port>/management/apiversions   # Alpaca services
```

The binaries are unsigned pre-1.0, so SmartScreen shows an
"unrecognized app" warning on the interactive install — an accepted
finding (the moral equivalent of the Linux packages' accepted lintian
list). Azure Trusted Signing is the noted post-1.0 path.

**Config-gated services** (`sky-survey-camera`, `plate-solver`,
`calibrator-flats`, `session-runner`, `polar-align`) have no sensible
default config, so they install with start type *Manual* — the Windows translation of the
Linux units' `ConditionPathExists=` gating. Write
`%ProgramData%\rusty-photon\<svc>.json` by hand, then:

```powershell
sc.exe config rusty-photon-<svc> start= auto
sc.exe start rusty-photon-<svc>
```

**Serial-device drivers** (`ppba-driver`, `qhy-focuser`,
`pa-falcon-rotator`, `pa-scops-oag`, `dsd-fp2`, `star-adventurer-gti`)
validate their hardware eagerly at startup and exit if the device is
missing — by design, so a broken device is never advertised on the
network. Until the device is attached (and its COM port matches the
config — the Windows default is `COM3`), the service sits in a
restart-every-5s loop driven by the failure actions; it comes up by
itself once the hardware appears. The cameras and the network-only
services serve with no hardware attached.

## First wiring: rusty-photon-doctor

The installer puts bytes on disk; doctor wires the configs (ADR-016).
After installing — or later modifying — the feature set, run once from an
elevated prompt:

```powershell
& "$env:ProgramFiles\rusty-photon\rusty-photon-doctor.exe"          # diagnose
& "$env:ProgramFiles\rusty-photon\rusty-photon-doctor.exe" --fix    # converge
```

Services pick fixed configs up on their next restart
(`Restart-Service rusty-photon-*`). The `rusty-photon-renew` Scheduled
Task runs `tls renew` daily as LocalSystem — a no-op until certificates
exist and are inside their renewal window; running services pick renewed
certificates up without a restart (mtime-triggered in-process reload).
Verify the task with `schtasks /Query /TN rusty-photon-renew`.

## Upgrading

Install the newer MSI — it performs a major upgrade: the old version is
removed, feature selections carry over, and self-created configs and logs
are untouched. Downgrades are blocked by the installer.

Pre-1.0, config schemas may change between versions with no in-service
migration — a service handed a config key it has retired refuses to start
rather than guessing. After upgrading, run `doctor --fix` and restart the
services (§First wiring): retired keys are deleted for you and the install
re-converges. The nightly channel proves this exact path every night (see
below).

## Nightly channel

CI publishes a rolling **`nightly` prerelease** built from the HEAD of
`main` whenever it has changed since the last publish; the suite MSI is
one of its assets, alongside the Linux packages (channel semantics and
the stable-URL `SHA256SUMS.txt` index:
[docs/packaging.md](packaging.md#nightly-channel)). Before anything is
published, the MSI passes the full lifecycle verification on a fresh
Windows runner — including installing over the previously published
nightly and running the shipped doctor's `--fix` (§Upgrading), so the
upgrade-and-migrate path below is proven every night.

```powershell
gh release download nightly --repo rusty-photon/rusty-photon --pattern '*.msi'
msiexec /qn /i rusty-photon-<fullversion>-x64.msi ADDLOCAL=ALL
```

The MSI filename carries the full nightly version
(`rusty-photon-<base>+nightly.<datetime>.g<sha>-x64.msi`), but Windows
Installer's numeric ProductVersion cannot: nightlies author
`<base>.<YYDDD>` (two-digit year × 1000 + day-of-year), and Windows
Installer compares only the first three fields, so upgrade logic sees
every nightly — and the `<base>` release — as the same version. The
installer therefore allows same-version upgrades: any nightly installs
in place over any other, over the `<base>` release, and vice versa,
with feature selections and configs carried over as on any upgrade.
Programs & Features shows the dated `<base>.<YYDDD>` as the version and
the full nightly string in the entry's Comments — the Comments, not
ProductVersion, tell nightlies apart.

Unlike apt's nightly channel, returning to the stable release needs no
special flag — same-version upgrades cut both ways, so the `<base>`
release MSI installs straight over any nightly. Rolling back to a
known-good nightly (the channel keeps no history): rebuild that commit
and install it:

```powershell
git checkout <known-good-sha>
scripts\build-msi.ps1 -NightlyVersion "<base>+nightly.<datetime>.g<short-sha>"
```

(`<base>` = the workspace version at that commit — the stamp's base must
match it, the build refuses a mismatch.)

## Configuration

The MSI ships no config files. Daemons self-create their config on first
start at `%ProgramData%\rusty-photon\<svc>.json` (the Windows analogue of
the Linux `/etc/rusty-photon` path). Exceptions: the config-gated five
(above) never write one; the two cameras, `zwo-focuser`, and
`phd2-guider` run on built-in defaults without writing a file until
settings are saved (via ui-htmx `config.apply`) or one is created by
hand. To change settings:

```powershell
notepad $env:ProgramData\rusty-photon\<svc>.json
sc.exe control rusty-photon-<svc> paramchange   # reload-capable services
Restart-Service rusty-photon-<svc>              # the rest
```

Reload-capable (SCM `ParamChange`, the SIGHUP analogue): filemonitor,
ppba-driver, qhy-focuser, sky-survey-camera, pa-falcon-rotator,
pa-scops-oag, dsd-fp2, star-adventurer-gti, qhy-camera, zwo-camera.
Services with `config.apply` support (via ui-htmx) rewrite these files at
runtime — hand-edits and UI edits share the same file.

**ui-htmx config**: self-created like every other service — the default
(the required `rp` target on localhost) is correct for every install,
since the device list lives in rp's equipment roster, not in ui-htmx
config (#569). The former install-time driver-map seed action is gone
with the map itself. **Upgrading an install whose config predates #569**
(it was seeded with a `drivers` map): the new service refuses to start
until the retired top-level `drivers` key is deleted from
`%ProgramData%\rusty-photon\ui-htmx.json` — the pre-1.0 fail-loudly
contract; doctor's `config.retired-keys` fix does the deletion where
doctor is available.

## Logs

Services log to rolling files
`%ProgramData%\rusty-photon\logs\<svc>.<date>.log` (daily rotation, 14
files retained) — under the SCM there is no usable stderr. Console runs
(`rusty-photon-<svc>.exe` from a terminal) log to stderr unchanged.

## Camera specifics

**qhy-camera** — QHYCCD's SDK is proprietary and never redistributed
(ADR-013). Install [QHY's All-in-One driver
pack](https://www.qhyccd.com/download/) first (needed for the signed
camera driver anyway); it provides the `qhyccd.dll` the service
delay-loads at startup. Without it the service logs an actionable
"qhyccd.dll not found" pointer and stops cleanly instead of crashing in
the loader. The Start-Menu shortcut **QHY Camera Doctor** (or
`rusty-photon-qhy-camera.exe doctor` in a console) diagnoses the
driver-pack/DLL state and reports the SDK version — note the service
builds against a pinned SDK, so the doctor's version report is the tool
for spotting ABI skew against whatever the All-in-One installed. Caveat:
`qhyccd.dll` itself needs `OpenCL.dll`, which ships with GPU drivers, not
Windows — on a box with no GPU driver the preflight fails even with the
All-in-One installed, and the doctor makes that visible.

**zwo-camera / zwo-focuser** — each feature bundles its own MIT-licensed
SDK DLL (`ASICamera2.dll` / `EAF_focuser.dll`, license in the install
dir), so nothing extra is needed for the *software* (ADR-014). ZWO
cameras additionally need [ZWO's signed camera driver
installer](https://www.zwoastro.com/downloads/); the EAF speaks inbox HID
and needs no vendor driver.

## plate-solver: ASTAP · phd2-guider: PHD2

Both wrap external programs that are deliberately not bundled: install
[ASTAP](https://www.hnsky.org/astap.htm) (plus a star database) and point
`astap_binary_path` / `astap_db_directory` in `plate-solver.json` at it;
install [PHD2](https://openphdguiding.org/) for phd2-guider.

## Removing

Remove single features (Apps → Installed apps → Modify, or
`msiexec /qn /i rusty-photon-<version>-x64.msi REMOVE=ZwoCamera`) or
uninstall entirely:

```text
msiexec /qn /x rusty-photon-<version>-x64.msi
```

Uninstall stops and deletes the services and removes the binaries, but
leaves self-created configs and logs in `%ProgramData%\rusty-photon`
(parity with `apt-get remove`). The "purge" analogue is manual: delete
that folder.

## Building and verifying the MSI

From a repo checkout on an x86_64 Windows box with Rust (MSVC host) and
the .NET SDK:

```powershell
scripts\build-msi.ps1                    # stage SDKs, build, wix build
scripts\build-msi.ps1 -SkipSdkStaging    # offline rebuild from cache
scripts\build-msi.ps1 -SkipBuild         # re-run wix only (installer loop)
scripts\build-msi.ps1 -NightlyVersion "<base>+nightly.<datetime>.g<sha>"
                                         # nightly stamp (see Nightly channel)
scripts\verify-msi.ps1                   # elevated, on a disposable box
scripts\verify-msi.ps1 -Msi dist\<fullversion>\rusty-photon-<fullversion>-x64.msi
                                         # (nightly builds land under the FULL
                                         # version; the -Msi default assumes a
                                         # release build)
```

(`-UpgradeFrom <prior msi>` installs a previously published MSI first, so
the main install runs as an in-place upgrade over it, then runs the
shipped doctor's `--fix` — the §Upgrading migration path — before the
lifecycle asserts, which probe provisioned services over HTTPS with the
observatory credential. Needs PowerShell 7. The mode was suspended
2026-07-18 while doctor was not yet in the packages and re-enabled once
D7 shipped it,
[#582](https://github.com/rusty-photon/rusty-photon/issues/582).)

`build-msi.ps1` stages the pinned native SDKs (QHYCCD import lib for the
delay-load link; the ZWO MIT DLLs that become payloads), release-builds
all services CRT-static (no VC++ redistributable needed), and runs WiX
v5 over `installer/`. Artifacts land in `dist/<version>/` with a
`SHA256SUMS.txt`. `verify-msi.ps1` proves the full lifecycle — silent
install, per-service-class checks, failure-actions proofs, feature
remove, uninstall — and expects a box with no prior rusty-photon state
(CI uses `windows-latest`; don't run it on your imaging box).

CI runs both scripts in the `msi` workflow (PRs touching the packaging
inputs, plus `workflow_dispatch`) and in `release.yml`, where
`verify-msi.ps1` gates the release upload. The nightly channel's `msi`
leg (`nightly-packages.yml`; see [Nightly channel](#nightly-channel))
runs them on a schedule with the nightly version stamp — so packaging
rot (a vendor SDK URL going stale, a runner image change) surfaces
between releases rather than at the next one.

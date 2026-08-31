# installer/ — the Windows suite MSI (WiX v5)

One `rusty-photon-<version>-x64.msi` for the whole family (ADR-015;
design + phase log: `docs/plans/windows-packaging.md`). Hand-authored WiX v5
sources, no generator — the same explicitness rule as the Linux
`services/<svc>/pkg/` dirs: `git grep` must find the real bytes that ship.

## Layout

- `Package.wxs` — product, stable UpgradeCode + major upgrades, directory
  layout (`Program Files\rusty-photon`, `%ProgramData%\rusty-photon` +
  `\logs`), the `WixUI_FeatureTree` UI, the feature tree (Core / Drivers /
  Automation), and the `rusty-photon-renew` scheduled-task custom actions
  (daily `rusty-photon-doctor.exe tls renew` as LocalSystem, registered
  via `schtasks` — WiX has no native scheduled-task element; removed on
  uninstall, rollback-guarded). The former ui-htmx seed custom action is
  gone (#569).
- `fragments/<svc>.wxs` — one fragment per packaged service (the Linux
  package set plus session-runner). Each installs
  `rusty-photon-<svc>.exe` (renamed from the Cargo bin), a Windows service
  named `rusty-photon-<svc>` running as LocalSystem with `--service`,
  restart-after-5s failure actions **plus the failure-actions flag** (see
  below), and a per-port firewall exception. The two zwo fragments bundle
  their MIT SDK DLL (ADR-013/014); `zwo-sdk-license.wxs` is the shared
  license component. The sentinel fragment additionally installs
  `rusty-photon-doctor.exe` (doctor ships in Core with sentinel — no
  separate package; the renewal scheduled task in `Package.wxs` runs it).
- `License.rtf` — the MIT license text shown by the installer UI
  (the family is MIT OR Apache-2.0; both full texts install with Core).

`scripts/check-pkg-assets.sh` asserts the fragment contract (fragment per
service, service name, exe rename, `--service`, failure actions + flag,
firewall port, demand-start on exactly the gated five, the doctor
exe + renewal-task wiring,
QHY/ZWO pin parity). Run it after any edit here.

## Fragment contract notes

- **Failure actions come in two elements.** `util:ServiceConfig` sets the
  restart-after-5s actions (systemd `Restart=on-failure`/`RestartSec=5`
  parity), and the *native* `ServiceConfig` element sets
  `SERVICE_CONFIG_FAILURE_ACTIONS_FLAG` via
  `FailureActionsWhen="failedToStopOrReturnedError"` — without it a clean
  eager-validation exit (`SERVICE_STOPPED` + `ServiceSpecific(1)`, see
  `rusty-photon-service-lifecycle`) never triggers the restart. WiX warns
  WIX1149 on the native element (an MSI SDK caveat about its *other* config
  types); `build-msi.ps1` suppresses it — the util element cannot express
  the flag, and `verify-msi.ps1` behaviorally proves the combination works.
- **Demand-start** (`Start="demand"`, no `Start="install"`) is the
  `ConditionPathExists=` translation for the five no-defaultable-config
  services: sky-survey-camera, plate-solver, calibrator-flats,
  session-runner, polar-align.
- **zwo-focuser's DLL keeps ZWO's original name** `EAF_focuser.dll`: the
  import library embeds the DLL name it was generated from, so the exe's
  import table asks the loader for that exact name (the `EAFFocuser.lib`
  rename exists only for the `-lEAFFocuser` link directive).
- **qhy-camera ships no DLL** (ADR-013): `qhyccd.dll` comes from QHY's
  operator-installed All-in-One pack; the exe delay-loads it and the
  fragment adds a Start-Menu shortcut for the interactive `doctor`.

## Building and verifying

```powershell
scripts\build-msi.ps1     # stage SDKs, cargo build (CRT-static), wix build
scripts\verify-msi.ps1    # elevated: install -> class checks -> uninstall
```

CI: the `msi` workflow (`.github/workflows/msi.yml`) runs both on
`windows-latest` on PRs touching the packaging inputs and on demand;
`release.yml` runs the same two scripts with `verify-msi.ps1` gating the
release upload; and the `msi` leg of `nightly-packages.yml` runs them
nightly with `build-msi.ps1 -NightlyVersion`, installing over the
previously published nightly MSI with the shipped doctor's `--fix` in
the loop (`verify-msi.ps1 -UpgradeFrom`;
`docs/plans/nightly-releases.md` N3).
Operator guide: `docs/packaging-windows.md`.

The build always defines two preprocessor variables: `Version` (the MSI
ProductVersion — the workspace version, or `<base>.<YYDDD>` on a nightly,
whose 4th field Windows Installer ignores when comparing) and
`FullVersion` (the human string for ARP comments — the same value on a
release, the full `+nightly.<datetime>.g<sha>` string on a nightly). That two-
field split is why `Package.wxs` sets `AllowSameVersionUpgrades`: every
nightly compares equal to its base release, so same-version upgrades must
be in range for nightly-over-nightly installs to upgrade in place.

On Linux, `wix build` (the `wix` dotnet tool + the same three extensions)
compiles the sources far enough to catch schema errors, but fails the bind
with spurious `WIX0389` "not a relative path" errors on `Directory/@Name` —
a known non-Windows limitation; full builds need a Windows box.

## Silent install recipes

```text
msiexec /qn /i rusty-photon-<version>-x64.msi ADDLOCAL=ALL
msiexec /qn /i rusty-photon-<version>-x64.msi ADDLOCAL=Core,Drivers,ZwoCamera,ZwoFocuser
msiexec /qn /i rusty-photon-<version>-x64.msi REMOVE=ZwoCamera
msiexec /qn /x rusty-photon-<version>-x64.msi
```

Feature IDs are the PascalCase service names (`ZwoCamera`, `QhyFocuser`,
`Rp`, …) plus the group features `Core` (required), `Drivers`, `Automation`.
Uninstall leaves self-created configs and logs in
`%ProgramData%\rusty-photon` (deb `remove` parity); deleting that folder is
the manual "purge".

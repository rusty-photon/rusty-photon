# ADR-015: Windows packaging — one MSI suite, per-service Windows services

## Status

Accepted (2026-07-11); implementation tracked by
[`docs/plans/archive/windows-packaging.md`](../plans/archive/windows-packaging.md).
Amends [ADR-012](012-service-packaging-architecture.md)'s formats clause
("MSI and Homebrew remain filemonitor-only"): a Windows deployment need now
exists, and the family ships a single Windows installer. The filemonitor-only
MSI (cargo-wix, WiX v3) is retired. ADR-013's native-SDK payload policy is
unchanged and extended to Windows below. Homebrew/macOS remains deferred
(PR-7 of the service-packaging plan).

## Context

The Linux family ships as 17 per-service `.deb`/`.rpm` packages (ADR-012)
because apt granularity, byte-identical maintainer scripts, and per-unit
systemd lifecycle make per-service packages natural there. Windows is a
different audience and a different platform:

- **The realistic Windows persona is a N.I.N.A. user** running a Windows
  imaging box who wants some of our Alpaca drivers (plus the config UI and
  sentinel), not necessarily the rp orchestrator. Windows users expect one
  installer with checkboxes, not 18 MSI downloads.
- **Windows has no analogue of several load-bearing Linux mechanisms** the
  packages rely on: systemd `Restart=`/`RestartSec=` (the serial drivers'
  eager-exit-until-hardware-appears design depends on it), journald capturing
  stderr, `ConditionPathExists=` unit gating, the shared system user's XDG
  home, udev rules, and the `/etc/rusty-photon` symlink. Each needs an
  explicit Windows decision, not an accidental default.
- **The binaries cannot yet run as Windows services.** The
  `rusty-photon-service-lifecycle` crate has an `scm` feature (SCM dispatch,
  Stop → `Shutdown`, ParamChange → `ReloadSignal`), but only filemonitor
  enables it, and under SCM stderr is a dead handle — today every log line
  from a service-mode process would vanish.
- **The native SDKs land differently.** ZWO's MIT DLLs are redistributable
  like the Linux `.so`s, but the QHYCCD Windows SDK's `qhyccd.lib` is a 76 KB
  *import library* for a 6 MB `qhyccd.dll` — qhy-camera.exe dynamically links
  a proprietary DLL we must not ship (ADR-013). Both vendors additionally
  require their own signed Windows device drivers, which no third-party
  installer can sanely embed.

## Decision

1. **One suite MSI, WiX v5, hand-authored.** A single
   `rusty-photon-<version>-x64.msi` with a feature tree: **Core** (required:
   sentinel + ui-htmx), **Drivers** (optional, one sub-feature per driver),
   **Automation** (optional: rp, session-runner, plate-solver, phd2-guider,
   calibrator-flats). Any install that includes a driver therefore includes
   sentinel (the watchdog/restart manager) and ui-htmx (the config UI) —
   they are unconditionally installed, which is simpler and more useful than
   conditional feature logic. Authored as plain committed WiX v5 sources
   (`installer/`), built with the `wix` CLI + `WixToolset.Util.wixext` +
   `WixToolset.Firewall.wixext`; cargo-wix is dropped and the filemonitor v3
   WXS deleted. x86_64 only.
2. **Every daemon runs as a Windows service named `rusty-photon-<svc>`**
   (ADR-012 naming carried over; installed exes are renamed in the asset
   mapping, Cargo bin names unchanged). All 18 services adopt the lifecycle
   crate's `scm` feature and a `--service` flag. SCM failure actions
   (restart after 5 s, indefinitely) reproduce the systemd
   `Restart=on-failure`/`RestartSec=5` contract the serial drivers' eager
   hardware validation depends on. The no-defaultable-config services
   (`sky-survey-camera`, `plate-solver`, `calibrator-flats`, and — found
   during W4, since it had never been packaged anywhere before —
   `session-runner`) install with `Start='demand'` — the Windows translation
   of `ConditionPathExists=` gating. Reload-capable services keep working via SCM `ParamChange`
   (already translated to `ReloadSignal` by the crate).
3. **Services run as LocalSystem.** Deliberately the opposite of the Linux
   hardening posture, accepted for the hobby-rig stage: sentinel's
   `Restart-Service` restart commands, ProgramData write-back, and USB/COM
   device access all work with zero ACL machinery, honoring tenet #2 (no
   permission failures at 2 a.m.). Least-privilege accounts (LocalService +
   per-service ACLs) are a post-1.0 consideration, recorded here so the
   choice is conscious, not drift.
4. **Config and state are platform-dependent defaults in code, not
   installer artifacts.** `rusty-photon-config` resolves the default config
   path per platform: XDG on Unix (unchanged),
   `%PROGRAMDATA%\rusty-photon\<svc>.json` on Windows — one obvious operator
   folder, the moral equivalent of ADR-012's shared home +
   `/etc/rusty-photon` symlink. Self-creation, `config.apply` write-back,
   and the fail-fast explicit `--config` contract all work unchanged; the
   MSI ships no config files and passes no `--config` flag. Other defaults
   follow the same rule: serial device paths (`COM3` vs `/dev/ttyUSB0`),
   rp's data directory, log locations.
5. **Service-mode logging goes to rolling files.** In SCM mode the lifecycle
   crate writes `%PROGRAMDATA%\rusty-photon\logs\<svc>.<date>.log` (daily rotation,
   bounded retention) instead of the dead stderr handle. Console mode is
   unchanged.
6. **Native SDK payloads (ADR-013 applied to Windows):**
   - *ZWO*: each service's own MIT DLL is bundled in its feature (per-device,
     ADR-014), license in the install dir. ZWO's signed camera driver
     installer is a documented operator prerequisite (same class as
     ASTAP/PHD2).
   - *QHY*: `qhyccd.dll` is **never redistributed**. The operator installs
     QHY's All-in-One pack (needed for the signed device driver anyway),
     which provides the DLL. qhy-camera **delay-loads** the DLL
     (`/DELAYLOAD`), probes known All-in-One install locations at startup
     (`AddDllDirectory`), and on failure logs an actionable pointer and stops
     cleanly instead of dying in the loader before `main`. An interactive
     `rusty-photon-qhy-camera doctor` subcommand (Start-Menu shortcut)
     diagnoses driver-pack/DLL presence, reports the SDK version, and can
     open QHY's download page in a browser — something a session-0 service
     cannot do. The build still links against the pinned SDK's import lib;
     ABI skew against whatever DLL the All-in-One ships is an accepted risk,
     surfaced by the doctor's version report.
7. **Our binaries are CRT-static.** The Windows package build sets
   `RUSTFLAGS=-C target-feature=+crt-static` (the analogue of the Linux
   RUNPATH injection — build-script-free, uniform). No VC++ redistributable
   is needed for our exes; if the vendor DLLs demand one, the merge module
   is added then.
8. **The MSI owns the firewall.** Per-service inbound TCP exceptions on each
   service's port via the WiX firewall extension — without them nothing is
   reachable and every port in the family exists to be reached over the LAN.
9. **Upgrade/remove semantics mirror deb.** Stable UpgradeCode + major
   upgrades with feature-state migration; self-created configs and logs live
   in ProgramData untracked by the MSI, so upgrade and uninstall leave them
   behind (parity with `apt-get remove`); full cleanup ("purge") is a
   documented manual step, as on rpm.
10. **Unsigned, pre-1.0.** Code signing is out of scope; the SmartScreen
    "unrecognized app" warning is a documented accepted finding (the moral
    equivalent of the accepted lintian list). Azure Trusted Signing is the
    noted post-1.0 path.

## Consequences

- Windows parity requires code PRs before any installer work: SCM enablement
  across 17 services, rolling-file logging, platform-dependent defaults, and
  the qhy delay-load/doctor seam. These are cross-platform improvements
  (Linux behavior unchanged).
- One MSI means one version for the whole family — consistent with the
  workspace's lockstep versioning, and a user updating one driver updates
  them all.
- session-runner ships on Windows from day one (Automation feature) while
  its Linux `.deb` remains an open follow-up of the service-packaging plan —
  the first platform-coverage inversion; the Linux package should follow.
- The suite installer is hand-maintained WiX: adding a service means adding
  a feature fragment, and `scripts/check-pkg-assets.sh` grows Windows
  assertions (service name / exe / port / pin parity) to keep the WXS from
  drifting, as it does for `pkg/` on Linux.
- Relying on the All-in-One pack for `qhyccd.dll` trades a pinned-download
  helper (the Linux approach) for a version we do not control; the delay-load
  preflight and doctor exist to make that failure mode diagnosable rather
  than a loader error dialog.

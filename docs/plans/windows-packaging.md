# Windows Packaging Plan — one MSI suite for the whole family

## Goal

Ship a single Windows installer (`rusty-photon-<version>-x64.msi`) that
installs any subset of the service family as supervised Windows services on
an x86_64 Windows imaging machine — the archetype being a N.I.N.A. box that
wants some of our Alpaca drivers plus the config UI, with or without the rp
orchestrator. The architecture is recorded in
[ADR-015](../decisions/015-windows-packaging-architecture.md), which amends
[ADR-012](../decisions/012-service-packaging-architecture.md)'s
"MSI stays filemonitor-only" clause;
[ADR-013](../decisions/013-native-sdk-payload-policy.md)'s payload policy is
unchanged and applied to Windows here. The Linux `.deb`/`.rpm` story
([docs/packaging.md](../packaging.md), `docs/plans/service-packaging.md`) is
untouched.

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| W0 | This plan + ADR-015 | Merged | #490 |
| W1 | SCM enablement: rolling-file logging in service mode (lifecycle crate) + `scm` feature / `--service` flag in the 17 remaining services | Merged | #493 |
| W2 | Platform-dependent defaults: config path → `%PROGRAMDATA%` on Windows, serial `COM` defaults, rp data dir | Merged | #492 |
| W3 | qhy-camera Windows: `/DELAYLOAD` + startup preflight + `doctor` subcommand | Merged | #491 |
| W4 | WiX v5 suite (`installer/`), `scripts/build-msi.ps1` + `scripts/verify-msi.ps1`, `check-pkg-assets.sh` Windows assertions; ui-htmx driver-map seeding; `msi.yml` on-demand CI harness | Merged | #499 |
| W5 | `release.yml` suite-MSI job + install-smoke gate; retire filemonitor `wix/` + cargo-wix; `docs/packaging-windows.md` (nightly verify moved to [nightly-releases.md](nightly-releases.md) N3) | Merged | #504 |

W1–W3 are pure code PRs (cross-platform, Linux behavior unchanged) and are
each independently useful; W4–W5 are the packaging layer. **W1–W3 can be
worked in parallel** (separate branches/worktrees) — they touch disjoint
crates except for two known overlap points to coordinate: (a) W1 and W3
both edit `services/qhy-camera`'s CLI surface (`--service` flag vs `doctor`
subcommand) — a small, mechanical rebase for whichever lands second; (b) W1's
log-file path and W2's config path both need "the Windows ProgramData dir" —
whichever lands first puts the one shared resolver in
`rusty-photon-service-lifecycle` or `rusty-photon-config`, the other reuses
it. W4's WiX fragments and scripts can be *authored* concurrently but can
only be verified and merged after W1–W3 are all in; W5 strictly follows W4.

## Decisions (fixed — see ADR-015 for rationale)

- **One suite MSI**, WiX v5, hand-authored in `installer/`, built by the
  `wix` CLI with the Util and Firewall extensions. cargo-wix and the
  filemonitor WiX v3 artifacts are retired. x86_64 only.
- **Feature tree:** Core (required: sentinel, ui-htmx) / Drivers (optional,
  per-driver sub-features) / Automation (optional: rp, session-runner,
  plate-solver, phd2-guider, calibrator-flats). Sentinel and ui-htmx are
  always installed.
- **Windows services named `rusty-photon-<svc>`**, exes installed as
  `rusty-photon-<svc>.exe` (asset-mapping rename; Cargo bin names
  unchanged), running as **LocalSystem**, `Start='auto'` — except the three
  no-defaultable-config services, which are `Start='demand'` (the
  `ConditionPathExists=` translation). SCM failure actions restart failed
  services after 5 s, indefinitely.
- **Config is platform-dependent in code, not shipped**: default path
  `%PROGRAMDATA%\rusty-photon\<svc>.json` on Windows (XDG on Unix,
  unchanged); no `--config` in service arguments; self-creation +
  `config.apply` write-back work as on Linux. Serial defaults become `COM3`
  on Windows; rp's data directory lands under
  `%PROGRAMDATA%\rusty-photon\rp\`.
- **Service-mode logs** are rolling files under
  `%PROGRAMDATA%\rusty-photon\logs\`.
- **SDK payloads:** ZWO MIT DLLs bundled per feature (ADR-013/014); QHY's
  `qhyccd.dll` **not** shipped — provided by the operator-installed QHY
  All-in-One pack, found via delay-load + preflight probing, diagnosed by a
  `doctor` subcommand. Vendor signed device drivers (ZWO camera driver, QHY
  All-in-One) are documented prerequisites, same class as ASTAP/PHD2.
- **`+crt-static`** for our exes; VC++ merge module only if the vendor DLLs
  turn out to need it.
- **Firewall exceptions** per service port, installed with each feature.
- **Unsigned pre-1.0**; SmartScreen warning documented as accepted.
- **Verification** = `scripts/verify-msi.ps1` on `windows-latest`, run as a
  release gate, nightly, and on demand (`workflow_dispatch`).

## Design

### Feature tree

```
rusty-photon-<version>-x64.msi
├── Core (required, not deselectable)
│   ├── sentinel            :11114   watchdog + restart API + notifications
│   └── ui-htmx             :11120   web config UI
├── Drivers (optional, pick per device; all off by default)
│   ├── ppba-driver         :11112   serial
│   ├── qhy-focuser         :11113   serial
│   ├── star-adventurer-gti :11117   UDP to mount
│   ├── pa-falcon-rotator   :11118   serial
│   ├── dsd-fp2             :11119   serial
│   ├── qhy-camera          :11121   USB; qhyccd.dll via All-in-One (below)
│   ├── zwo-camera          :11122   USB; bundles ASICamera2.dll
│   ├── pa-scops-oag        :11123   serial
│   ├── zwo-focuser         :11124   USB; bundles its EAF DLL
│   ├── filemonitor         :11111   SafetyMonitor
│   └── sky-survey-camera   :11116   simulator; demand-start (gated)
└── Automation (optional, off by default)
    ├── rp                  :11115   orchestrator
    ├── session-runner      :11171   workflow-DSL runner; demand-start (gated)
    ├── plate-solver        :11131   demand-start (gated); needs ASTAP
    ├── phd2-guider         :11130   needs PHD2
    └── calibrator-flats    :11170   demand-start (gated)
```

- Installer UI is the stock feature-tree dialog set (`WixUI_FeatureTree`);
  silent installs select features with
  `msiexec /qn /i ... ADDLOCAL=Core,ZwoCamera,Rp` (exact feature IDs in
  `installer/`).
- Install layout is flat: `C:\Program Files\rusty-photon\` holds every
  selected `rusty-photon-<svc>.exe`, the ZWO DLLs next to their exes
  (distinct filenames per ADR-014 — co-install is conflict-free), and the
  ZWO license file. No PATH manipulation (the old filemonitor WXS's PATH
  component is not carried over).
- `session-runner` shipped here from day one; its Linux `.deb`/`.rpm` and
  macOS tarball closed the follow-up noted in
  `docs/plans/service-packaging.md` (#699 — config-gated, port 11171).
- A future `zwo-filterwheel` (ADR-014) becomes one more Drivers sub-feature.

### Windows service model (W1)

All 18 services get what filemonitor already has:

- `rusty-photon-service-lifecycle` `scm` feature in `Cargo.toml`
  (Windows-only dep; costs nothing elsewhere) and a `--service` flag passed
  by the MSI's `ServiceInstall` `Arguments`, wired to
  `ServiceRunner::scm_mode` — SCM Stop maps to `Shutdown`, ParamChange to
  `ReloadSignal`, so reload-capable services are reloadable via
  `sc control rusty-photon-<svc> paramchange` with no new code.
- **Failure actions** (WiX `util:ServiceConfig`): first/second/third failure
  = restart, 5 s delay, reset period 1 day. This restores the systemd
  `Restart=on-failure`/`RestartSec=5` contract the serial drivers'
  eager-exit design requires. **As built (W1):** on a run-closure error the
  SCM wrapper reports `SERVICE_STOPPED` with
  `ServiceExitCode::ServiceSpecific(1)` — deterministic, keeps the real
  cause in the SCM stop record — so the installer MUST pair the restart
  failure actions with the failure-actions-on-error flag
  (`SERVICE_CONFIG_FAILURE_ACTIONS_FLAG`; `sc.exe failureflag` or a small
  custom action if `util:ServiceConfig` cannot express it). Verify with a
  kill-and-observe test in `verify-msi.ps1`.
- **Demand-start gating:** `sky-survey-camera`, `plate-solver`,
  `calibrator-flats` install with `Start='demand'` and (unlike the rest) no
  `ServiceControl` start on install. `docs/packaging-windows.md` documents:
  write `%PROGRAMDATA%\rusty-photon\<svc>.json`, then
  `sc start rusty-photon-<svc>` (or Services.msc).
  **As built (W4): `session-runner` is the fourth gated service** — its
  `workflows_dir`/`state_dir` are required config fields with no usable
  defaults (`docs/services/session-runner.md`), which nothing had surfaced
  before because it has no Linux package; the first full `verify-msi.ps1`
  run caught it crash-looping on the missing config. Its future Linux `.deb`
  gets the matching `ConditionPathExists=` unit.
- **Logging:** in SCM mode the lifecycle crate's `init_service_tracing`
  (new in W1; console mode delegates to the unchanged `init_tracing`) swaps
  stderr for a `tracing-appender` rolling file
  (`%PROGRAMDATA%\rusty-photon\logs\<svc>.<date>.log` — daily rotation adds
  the date component; keep 14 files), with a non-blocking writer whose
  guard is held to process exit so the final lines flush on Stop. Console mode is byte-for-byte unchanged.
  **As built (W1):** the crate exposes a sticky process-global
  `is_scm_service()`, and all 18 `bound_addr=` stdout-handshake sites (15
  service libs + 3 mains) are gated on it; raw stderr error prints on
  service-mode paths were routed through tracing. Rust's Windows stdio
  sinks absent/NULL std handles without panicking, so this is about not
  *losing* diagnostics; the real-SCM std-handle smoke stays a
  `verify-msi.ps1` item. Bazel-side, the lifecycle crate is ONE library
  target whose `crate_features` selects `scm` on Windows — per-consumer
  `_scm` variants caused dual crate instantiation (E0308) on Windows and
  must not be reintroduced.

### Platform-dependent defaults (W2)

- `rusty-photon-config::resolve_config_path` gains a `cfg(windows)` default:
  `%PROGRAMDATA%\rusty-photon\<svc>.json` (via the `ProgramData` environment
  variable with `C:\ProgramData` fallback), replacing the per-user
  `ProjectDirs` path that would vanish into
  `...\systemprofile\AppData\Roaming` under a service account. Everything
  downstream — `resolve_and_init` self-creation, `materialize_identity`,
  `config.apply` write-back, the explicit-`--config`-missing-file hard
  error — is untouched. Unix behavior is untouched.
- Driver serial defaults: `cfg(windows)` → `COM3` (vs `/dev/ttyUSB0` etc.).
  Cosmetic honesty, not magic — drivers still restart-loop until the
  operator sets the real port, exactly as on Linux.
- rp's scaffold config: `session.data_directory` defaults under
  `%PROGRAMDATA%\rusty-photon\rp\` on Windows.
- The MSI pre-creates `%PROGRAMDATA%\rusty-photon\` and `...\logs\` (empty
  dirs, LocalSystem-writable by ProgramData's default ACL, admin-editable).

### qhy-camera on Windows (W3)

The QHYCCD Windows SDK's `qhyccd.lib` is an import library — the exe needs
the proprietary `qhyccd.dll` at runtime, which ADR-013 forbids shipping.
The operator installs QHY's All-in-One pack regardless (it carries the
signed device driver); it also provides the DLL. To make the missing-DLL
case diagnosable instead of a pre-`main` loader failure:

- Delay-load link args (`/DELAYLOAD:qhyccd.dll` + `delayimp.lib`), Windows
  msvc real-variant only. **As built (W3 finding): these live in
  `services/qhy-camera/build.rs` — the binary crate — NOT in
  `libqhyccd-sys/build.rs` as this plan originally said.**
  `cargo:rustc-link-arg` applies only to the emitting package's own link
  targets and does not propagate from a dependency's build script to the
  final binary (verified empirically; rules_rust likewise forwards only
  `-l`/`-L`). The hand-written BUILD.bazel mirrors the flags via
  `rustc_flags` selects on the real-SDK targets; a pointer comment in
  libqhyccd-sys's Windows arm guards against moving them back.
- Startup preflight (service + console): probe the All-in-One's known
  install directories (enumerated during W3 on a real Windows box — flagged
  unknown below) plus the exe's own directory, `AddDllDirectory` the first
  hit, and attempt a `LoadLibrary`. On failure: one distinctive error log
  naming the All-in-One download URL, then a clean non-zero exit (SCM
  restarts every 5 s — same contract as a missing serial device; the unit
  comes up by itself once the pack is installed).
  **As built (W4 correction):** the preflight must run *inside* the
  `ServiceRunner` run closure, not in `main` before dispatch — the SCM
  wrapper reports `Running` before the closure, which is what makes the
  exit clean; a pre-registration exit is an SCM start failure and aborts
  the whole MSI install with error 1920 during `StartServices`
  (W4's first full `verify-msi.ps1` run caught exactly this).
- `rusty-photon-qhy-camera doctor` (interactive subcommand, Start-Menu
  shortcut): reports device-driver presence, DLL location and
  `GetQHYCCDSDKVersion` vs. the pinned build-time SDK version (ABI-skew is
  an accepted risk — the doctor makes it visible), and can open the QHY
  download page in the default browser (a session-0 service cannot).
- BDD/unit coverage: preflight path selection and doctor report rendering
  are testable cross-platform behind the existing FFI-mock seam; the real
  LoadLibrary path is exercised by the on-Windows verification pass.

zwo-camera/zwo-focuser need none of this: their MIT DLLs ship in the MSI
next to the exes (the loader finds same-directory DLLs first), one DLL per
service per ADR-014.

### Installer authoring & build (W4)

- `installer/` holds hand-authored WiX v5 sources: `Package.wxs` (product,
  `MajorUpgrade` with stable UpgradeCode + feature migration, UI, directory
  + ProgramData layout) plus one fragment per service (component: exe [+
  DLL/license for zwo], `ServiceInstall`/`ServiceControl`,
  `util:ServiceConfig` failure actions, `fw:FirewallException` for its
  port). Plain committed files, no generator — same explicitness rule as
  `services/<svc>/pkg/` (`git grep` must not lie).
  **As built (W4):** the failure-actions-on-error flag needs no custom
  action — the *native* WiX `ServiceConfig` element expresses
  `SERVICE_CONFIG_FAILURE_ACTIONS_FLAG` declaratively
  (`FailureActionsWhen="failedToStopOrReturnedError"`, paired with
  `util:ServiceConfig` for the restart actions; the WIX1149 advisory on the
  native element is suppressed with rationale in `build-msi.ps1`). And
  zwo-focuser's bundled DLL must keep ZWO's original name
  `EAF_focuser.dll`: an import library embeds the DLL name it was generated
  from, so the exe's import table asks the loader for that exact name — the
  `EAFFocuser.lib` rename exists only for the `-lEAFFocuser` link directive.
- `scripts/check-pkg-assets.sh` grows Windows assertions: every packaged
  service has a fragment; fragment service name = `rusty-photon-<dir>`;
  exe rename mapping matches; port in the firewall rule matches the
  service's documented port; demand-start on exactly the gated four; the
  QHY SDK version pinned for the Windows build matches the Linux pins; the
  ZWO blob ref matches `install-zwo-sdk`'s default.
- **ui-htmx driver-map seeding** (post-W0 finding): ui-htmx discovers
  drivers only through the static `drivers` map in its own config — no
  discovery — and its self-created default lists a single `dsd-fp2`
  entry, so a fresh install would show none of the actually-selected
  drivers. The MSI uniquely knows the selection: on install, **iff**
  `%PROGRAMDATA%\rusty-photon\ui-htmx.json` does not exist, write one
  with an entry per selected driver feature (fixed localhost ports —
  fully deterministic). Only-if-absent preserves the self-creation /
  `config.apply` ownership model; upgrades never touch it.
  **As built (W4):** a deferred custom action (after `InstallFiles`, before
  `StartServices` — ui-htmx would otherwise self-create its default first)
  runs `installer/seed-ui-htmx-config.ps1`, whose ground truth is the
  installed `rusty-photon-*.exe` set — the transaction's end state — so no
  feature-state property plumbing is needed. Each entry seeds `base_url` +
  `device_type` (ui-htmx's `device_type` default is `covercalibrator`,
  wrong for everything else). A Core-only install seeds an empty map
  (honest, unlike the phantom dsd-fp2 default), and a co-installed rp
  feature seeds the `rp` target, enabling `/equipment` and `/stream`.
  **Retired 2026-07-18** ([#569](https://github.com/rusty-photon/rusty-photon/issues/569)):
  the drivers map itself is gone — device targets derive from rp's
  equipment roster, ui-htmx's self-created default (the required `rp`
  target) is correct for every install, and the seed action + script were
  deleted; `verify-msi.ps1` now asserts the self-created shape instead.
- `scripts/build-msi.ps1` (runs on a dev box or CI, mirrors
  `build-packages.sh`): stage pinned SDKs into the package cache (QHY
  `sdk_win64_<ver>.zip` for `qhyccd.lib`; ZWO DLLs from the pinned ref);
  `RUSTFLAGS="-C target-feature=+crt-static" cargo build --release` — the
  two zwo services in separate cargo invocations so feature unification
  cannot re-union their per-device SDK links (same rule as Linux); then
  `wix build` → `dist/<version>/rusty-photon-<version>-x64.msi` +
  SHA256SUMS entry.
- `scripts/verify-msi.ps1` (the `verify-packages.sh` analogue, on any
  Windows box / `windows-latest`): silent install with all features →
  every auto-start service `RUNNING` (`sc query`), gated four present but
  stopped → configs self-created in `%PROGRAMDATA%` with minted
  `unique_id`s → the self-created `ui-htmx.json` carries the required `rp`
  target and no retired `drivers` key (#569 — formerly a seeded-map
  assertion) → HTTP port probes (`/management/apiversions` for
  Alpaca services) → log files appearing under `...\logs\` → kill one service
  process and observe SCM restart it (failure-actions proof) → feature
  remove → full uninstall: services gone, Program Files clean, configs and
  logs still present (documented "purge by hand" step deletes them).
  qhy-camera on a runner (no All-in-One): assert the preflight's
  distinctive error in its log — the documented no-DLL behavior, not a
  loader crash.

### CI & release (W5)

- `release.yml`: replace the filemonitor `build-windows`/cargo-wix job with
  a suite job on `windows-latest` — `build-msi.ps1`, then `verify-msi.ps1`
  as the gate, then attach the MSI + checksums to the release. Delete
  `services/filemonitor/wix/` and the cargo-wix install step. The Linux
  matrix and the deferred Homebrew items (PR-7) are untouched.
  **As built (W5):** the suite job also asserts the pushed tag matches the
  workspace version (`build-msi.ps1` derives the MSI version from
  `Cargo.toml`, so a mismatched tag would ship wrongly-versioned binaries
  under a right-versioned tag). The artifact carries only the MSI — the
  release job already computes a global `SHA256SUMS.txt` across all
  platforms' artifacts; uploading the per-dist checksum file too would
  collide with it. `[package.metadata.wix]` was removed from filemonitor's
  Cargo.toml along with the `wix/` dir, and
  `docs/services/filemonitor.md`'s Windows install/registration sections
  now describe the suite MSI.
- Nightly: the scheduled `build-msi.ps1` + `verify-msi.ps1` run lands as
  the `msi` leg of `nightly-packages.yml` (phase N3 of
  [nightly-releases.md](nightly-releases.md)), not as a scheduler of its
  own — W5 delivers the scripts and the release-tag job; N3 wires the
  nightly leg, version stamping (`AllowSameVersionUpgrades`), and the
  nightly-over-nightly upgrade check. (The upgrade check was **suspended
  2026-07-18** — pre-1.0 config-schema churn reddened it with no product
  signal; re-enable with doctor `--fix` in the loop once D7 ships doctor
  in the packages, [#582](https://github.com/rusty-photon/rusty-photon/issues/582).)
- `docs/packaging-windows.md` (operator guide, `docs/packaging.md` gets a
  pointer): install/upgrade/remove, feature selection incl. silent
  `ADDLOCAL` recipes, prerequisites (QHY All-in-One, ZWO camera driver,
  ASTAP, PHD2), the doctor, log locations, demand-start workflow, manual
  purge, and the accepted-findings list (SmartScreen warning, unsigned
  binaries).

## Verification

- `verify-msi.ps1` checks enumerated above, run on `windows-latest`
  (release gate + nightly) — the packaging-lifecycle layer.
- Real-hardware pass on a physical Windows box (analogue of the on-rig
  section in `service-packaging.md`): QHY All-in-One + camera → preflight
  finds the DLL, doctor reports versions, camera serves; ZWO camera + EAF →
  bundled DLLs, devices enumerate; a serial driver on a real COM port; a
  reload via `sc control ... paramchange`; results recorded here.
- ConformU against a Windows-served driver once, as a spot check (the
  drivers are the same code CI already conformance-tests on Linux).

## Flagged unknowns (resolve during the noted PR)

- [ ] (W3) Where the QHY All-in-One pack installs `qhyccd.dll` (and whether
      it adds itself to the system PATH) — enumerate the probe list on a
      real Windows machine; the preflight's known-locations list is seeded
      from that.
- [x] (W1) SCM failure-actions mechanics: resolved — `SERVICE_STOPPED` +
      `ServiceExitCode::ServiceSpecific(1)`; the installer must set
      `SERVICE_CONFIG_FAILURE_ACTIONS_FLAG` (now a W4 requirement, verified
      by the kill-and-observe check).
- [x] (W4) Whether the vendor DLLs require the VC++ redistributable:
      resolved — **no merge module needed**. All three
      (`ASICamera2.dll`, `EAF_focuser.dll`, `qhyccd.dll` 26.06.04) import
      only inbox system DLLs (no `VCRUNTIME140`/`MSVCP140`/ucrt apiset
      imports — statically linked CRTs). Note: `qhyccd.dll` also imports
      `OpenCL.dll`, which ships with GPU drivers, not Windows — on a
      GPU-driver-less box the preflight's `LoadLibrary` fails even with the
      All-in-One installed, and the doctor is the tool that makes that
      visible.
- [x] (W4) Whether ZWO's EAF needs the vendor driver installer: resolved —
      `EAF_focuser.dll` imports `HID.DLL` (+ SetupAPI), i.e. the EAF speaks
      inbox HID and needs no vendor driver; the prerequisite doc line covers
      cameras only. (Re-confirm incidentally during the real-hardware pass.)
- [ ] (W1→W4) Std-handle behavior under SCM: design-resolved (all 18
      `bound_addr=` handshake sites gated on `is_scm_service()`; Rust sinks
      NULL handles, so the risk was lost output, not a panic) — the
      remaining item is the real-SCM smoke in `verify-msi.ps1`.

## Future considerations

- Code signing via Azure Trusted Signing post-1.0 (kills the SmartScreen
  warning); a winget manifest becomes worthwhile once signed.
- LocalService + per-service ACLs if the hardening posture ever needs to
  match Linux.
- Windows Event Log entries for lifecycle events (started/stopped/crashed)
  alongside the rolling files.
- arm64 Windows if the ecosystem ever demands it.

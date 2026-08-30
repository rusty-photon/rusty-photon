# Workspace Design

Top-level reference for the rusty-photon workspace. This document indexes
project-wide documentation and captures workspace-level concerns that don't
belong in any single service design doc.

## Project Tenets

The tenets rank above feature work. When a design decision trades one of
these away, the decision is wrong.

1. **Make the best use of night time.** Clear, dark hours are the scarce
   resource; everything else (build time, code size, convenience) is
   negotiable against it.
2. **Robustness.** Unattended operation at 2 a.m. is the design point:
   fail-fast validation, invalid states made unrepresentable (typed
   newtypes), bad configs rejected at load — never mid-session.
3. **No actuation on connect.** Connecting a driver, starting or
   restarting a service, reconnecting after a transport glitch, and
   every passive/supervisory transition MUST NOT physically actuate
   hardware — no motion, park/unpark slews, homing, cover or lamp
   changes, cooler setpoints, power or dew-heater toggles, filter-wheel
   moves, or guide pulses. Actuation requires an operator or workflow
   decision. **Stop-class commands are always permitted** (halting
   in-flight motion, aborting an exposure — stopping is inherently
   safe), and automatic cleanup *inside* an operator-started session
   (abort/stop/park on a safety transition, warm-up on session end) is
   a workflow decision, not a violation. Corollaries: connect/reconnect
   handshake hooks stay read-only (they re-run on every serial glitch);
   `config.apply` must never push output states to hardware; a driver
   that cannot know where its axes are must never guess with the motors
   (see the anchored-frame rule in
   [star-adventurer-gti.md](services/star-adventurer-gti.md#park-lifecycle));
   vendor-SDK init side effects outside our control (e.g. QHY filter
   wheels auto-home at firmware level on `InitQHYCCD`) are documented
   in the owning service's design doc rather than silently accepted.
   Adopted 2026-07-20 after a connect-time park slewed a physically
   parked mount 90°/90° to a fabricated pose.

## Services

| Service | ASCOM Type | Port | Design Doc |
|---------|-----------|------|------------|
| [filemonitor](services/filemonitor.md) | SafetyMonitor | 11111 | `docs/services/filemonitor.md` |
| [ppba-driver](services/ppba-driver.md) | Switch + ObservingConditions | 11112 | `docs/services/ppba-driver.md` |
| [qhy-focuser](services/qhy-focuser.md) | Focuser | 11113 | `docs/services/qhy-focuser.md` |
| [phd2-guider](services/phd2-guider.md) | — (client library) | — | `docs/services/phd2-guider.md` |
| [sentinel](services/sentinel.md) | — (monitoring service) | 11114 | `docs/services/sentinel.md` |
| [rp](services/rp.md) | — (orchestrator) | 11115 | `docs/services/rp.md` |
| [plate-solver](services/plate-solver.md) | — (rp-managed service wrapping ASTAP) | 11131 | `docs/services/plate-solver.md` |
| [calibrator-flats](services/calibrator-flats.md) | — (orchestrator plugin) | 11170 | `docs/services/calibrator-flats.md` |
| [polar-align](services/polar-align.md) | — (orchestrator plugin) | 11172 | `docs/services/polar-align.md` |
| [sky-survey-camera](services/sky-survey-camera.md) | Camera (simulator) | 11116 | `docs/services/sky-survey-camera.md` |
| [qhy-camera](services/qhy-camera.md) | Camera (+ FilterWheel) — QHYCCD hardware | 11121 | `docs/services/qhy-camera.md` (implemented v0; native QHYCCD SDK dep — links `static=qhyccd` + `libusb-1.0`; **built + tested on GitHub-hosted Linux/macOS/Windows** via the `qhyccd-sdk-install@v3` action, plus the Pi nightly for linux-arm64. Vendored first-party (ADR-009); sanitized under `safety.yml` via the SDK-free `simulation` path (`QHYCCD_SKIP_NATIVE_LINK=1`) — only `bdd-infra` is excluded there) |
| [zwo-camera](services/zwo-camera.md) | Camera — ZWO ASI hardware | 11122 | Phase E (full Camera) landed: full `Device + Camera` over `zwo-rs` (exposure state machine, ROI/bin, gain/offset, cooling, readout, ST4 pulse-guiding), serial identity, config actions; 45 unit + 57 BDD green, ConformU passes. Bazel first-class (`lib`/`binary`/`unit_test`; `bdd`/`conformu` run under Bazel). The EFW FilterWheel is a future separate `zwo-filterwheel` service (ADR-014); this binary links only the ASI camera SDK (zwo-rs `camera` feature). ConformU is wired into `conformu.yml` (per-service matrix + `install-zwo-sdk`), and the nightly `native.yml` builds the real linked path on Linux/macOS/Windows; the `rp` `CameraConfig` consumer is the only Phase-G tail item left. Native ZWO SDK dep, gated out of the default build. See `docs/services/zwo-camera.md` + ADR-008 + ADR-014. |
| [star-adventurer-gti](services/star-adventurer-gti.md) | Telescope | 11117 | `docs/services/star-adventurer-gti.md` (implemented — `ITelescopeV3` subset: async slew, sync, sidereal tracking, software park, pulse guiding; all BDD scenarios green) |
| [pa-falcon-rotator](services/falcon-rotator.md) | Rotator + Switch (status) | 11118 | `docs/services/falcon-rotator.md` |
| [pa-scops-oag](services/pa-scops-oag.md) | Focuser | 11123 | `docs/services/pa-scops-oag.md` (Pegasus Astro Scops OAG — FTDI serial, Pegasus DMFC/Scops ASCII protocol at 19200 8N1; no temperature sensor) |
| [zwo-focuser](services/zwo-focuser.md) | Focuser | 11124 | `docs/services/zwo-focuser.md` (ZWO EAF — native SDK FFI via `zwo-rs`, mirrors `zwo-camera`'s architecture rather than the serial shared-transport pattern; v0 implemented 2026-07-09 — 25 unit + 26 BDD scenarios green, full quality gate green, ConformU wired; pending real-hardware validation) |
| [dsd-fp2](services/dsd-fp2.md) | CoverCalibrator | 11119 | `docs/services/dsd-fp2.md` (first adopter of `rusty-photon-shared-transport`) |
| [ui-htmx](services/ui-htmx.md) | — (web config UI / BFF, not an ASCOM device) | 11120 | `docs/services/ui-htmx.md` |
| [session-runner](services/session-runner.md) | — (generic workflow-orchestrator plugin) | 11171 | `docs/services/session-runner.md` (implemented — executes declarative JSON workflow documents against `rp`'s MCP tools: expression layer, trigger overlay, blackboard resume; ships `deep_sky.json`, `calibrator_flats.json`, `sky_flat.json`; authoring guide: [workflow-documents.md](references/workflow-documents.md)) |
| [doctor](services/doctor.md) | — (install diagnosis CLI, not an ASCOM device) | — | `docs/services/doctor.md` (complete — diagnosis: config parsing, port collisions, cross-service name joins, unit/privilege gaps, SDK-free hardware checks, per-service `doctor` aggregation; repair via `--fix`; the TLS + credential lifecycle including `tls renew`; catalog derived from `services/*/pkg/doctor.toml`; ships in sentinel's packages — [ADR-016](decisions/016-service-config-ownership-and-doctor.md)) |
| [svbony-camera](services/svbony-camera.md) | Camera — SVBony hardware | 11125 | v0 implemented 2026-07-21; **real-hardware validated 2026-07-26 against a physical SV605CC — ConformU (`alpacaprotocol` + full `conformance`) passes with zero errors/issues on the production real-SDK binary**. Validation resolved every open punch-list item (exposure unit = µs confirmed; no stale-frame flush needed; `CanStopExposure` stays `false`; `ElectronsPerADU` permanently `NOT_IMPLEMENTED`) and forced four driver changes: production enumeration registers real cameras (Phase E boundary removed), `MaxADU` = 65535 (SDK rescales 14-bit to full Raw16 scale), R4 aligned-down `CameraXSize`/`CameraYSize` (2976×3000), and a responsive abort (~0.3 s drain via cancel-flag poll slices). 74/80 unit tests + 64/64 BDD scenarios green. Packaging landed Phase G (`rusty-photon-svbony-sdk-install`, RUNPATH); the real `:svbony-camera` Bazel binary stays `manual` (Bazel-side SDK-fetch rule still deferred); dev-machine USB permissions gap filed as issue #710. See `docs/services/svbony-camera.md` + `docs/plans/archive/svbony-camera.md` + ADR-018. |
| [planetarium-bridge](services/planetarium-bridge.md) | Telescope (virtual) | 11126 | `docs/services/planetarium-bridge.md` (virtual target-entry device, NOT a mount: planetarium Align gestures become paused rp targets via `add_target`; slews are simulated motion, imports spool while rp is down; never touches hardware) |

## Documentation Index

| Document | Purpose |
|----------|---------|
| **Rules** | |
| [docs/AGENTS.md](AGENTS.md) | Rules for all AI agents and human operators (`CLAUDE.md` is a symlink to this file) |
| **Skills** (how-to playbooks — read before performing the respective task) | |
| [docs/skills/development-workflow.md](skills/development-workflow.md) | Skill: design-first, test-first development workflow |
| [docs/skills/testing.md](skills/testing.md) | Skill: writing and organizing tests (test pyramid, BDD, unit tests) |
| [docs/skills/pre-push.md](skills/pre-push.md) | Skill: running CI quality gates before pushing |
| [docs/skills/coverage.md](skills/coverage.md) | Skill: checking code coverage in CI and locally (`codecov/patch`, `codecov/project`) |
| [docs/skills/service-lifecycle.md](skills/service-lifecycle.md) | Skill: scaffolding a long-running service binary (`main.rs`, runtime + shutdown handling) |
| [docs/skills/archiving-plans.md](skills/archiving-plans.md) | Skill: archiving a completed plan into `docs/plans/archive/` |
| [docs/skills/bazel-remote-cache.md](skills/bazel-remote-cache.md) | Skill: using the self-hosted Bazel remote cache |
| [docs/skills/raspberry-pi-runner.md](skills/raspberry-pi-runner.md) | Skill: the Pi 5 self-hosted ARM64 nightly runner |
| **Crate design docs** (substantial workspace libraries — see [docs/crates/](crates/)) | |
| [docs/crates/rp-ephemeris.md](crates/rp-ephemeris.md) | `rp-ephemeris` — `Ephemeris` trait, ERFA wrapping, panic-safety + NaN-degradation, derived helpers, time-scale treatment |
| [docs/crates/rp-targets.md](crates/rp-targets.md) | `rp-targets` — `redb`-backed imaging-plan store: targets, acquisition goals, per-target grading-threshold + scheduling-constraint overrides; `TargetStore` trait. Design stage; crate not yet built. |
| [docs/crates/rusty-photon-service-lifecycle.md](crates/rusty-photon-service-lifecycle.md) | `rusty-photon-service-lifecycle` — unified tokio runtime + signal handlers + optional Windows SCM, exposing a single `Shutdown` handle across the workspace |
| **References** | |
| [docs/references/ascom-alpaca.md](references/ascom-alpaca.md) | ASCOM Alpaca protocol reference |
| [docs/references/skywatcher-motor-controller-command-set.md](references/skywatcher-motor-controller-command-set.md) | Sky-Watcher motor-controller wire protocol (USB + UDP/11880) — used by `star-adventurer-gti` |
| [docs/references/omnisim.md](references/omnisim.md) | OmniSim (ASCOM Alpaca Simulators) reference — used by BDD/integration tests |
| [docs/references/qhyccd-sdk-manual.md](references/qhyccd-sdk-manual.md) | QHYCCD SDK manual (unofficial English translation, V2.1) — used by `qhy-camera` |
| [docs/references/workflow-documents.md](references/workflow-documents.md) | Authoring guide for `session-runner` workflow documents: the format, the expression grammar, the re-entrancy contract, worked examples |
| [docs/services/config-actions.md](services/config-actions.md) | Cross-driver configuration protocol: the `config.get` / `config.apply` / `config.schema` ASCOM actions shared by every driver and consumed by `ui-htmx` |
| **Validation records** (real-hardware ConformU proof trail — see [docs/validation/](validation/)) | |
| [docs/validation/README.md](validation/README.md) | Index of successful real-hardware ConformU runs: per run, the exact commit tested, platform, device identity, and the unmodified ConformU output |
| [docs/svbony-camera-windows-install.md](svbony-camera-windows-install.md) | Operator guide: installing `svbony-camera` on Windows from source (vendor driver + SDK staging + MSVC build) until the MSI ships it |
| **Decisions** (Architecture Decision Records — see [docs/decisions/](decisions/)) | |
| [ADR-001](decisions/001-fits-file-support.md) | FITS file support |
| [ADR-002](decisions/002-tls-for-inter-service-communication.md) | TLS for inter-service communication |
| [ADR-003](decisions/003-authentication-for-device-access.md) | Authentication for device access |
| [ADR-004](decisions/004-testing-strategy-for-http-client-error-paths.md) | Testing strategy for HTTP-client error paths |
| [ADR-005](decisions/005-plate-solver.md) | Plate solver: ASTAP via subprocess + verification spike |
| [ADR-006](decisions/006-typed-physical-quantities-for-mount-pointing.md) | Typed physical quantities (newtypes) for mount pointing |
| [ADR-007](decisions/007-rusty-photon-driver-shared-crate.md) | Extract `rusty-photon-driver` — the shared ASCOM-driver adapter |
| [ADR-008](decisions/008-zwo-camera-native-sdk-ffi.md) | `zwo-camera` native ZWO SDK: author-maintained `zwo-rs`/`libzwo-sys` FFI + MIT-SDK public caching |
| [ADR-009](decisions/009-vendor-qhyccd-rs.md) | Vendor `qhyccd-rs` + `libqhyccd-sys` into the workspace (dual-homed) |
| [ADR-010](decisions/010-vendor-zwo-rs.md) | Vendor `zwo-rs` + `libzwo-sys` into the workspace (dual-homed) |
| [ADR-011](decisions/011-error-reporting-layers.md) | Layered error reporting — `thiserror` everywhere, `color-eyre` only at the binary boundary |
| [ADR-012](decisions/012-service-packaging-architecture.md) | System packaging architecture — native `.deb`/`.rpm` for all services (`rusty-photon-*` naming, XDG config, shared service user) |
| [ADR-013](decisions/013-native-sdk-payload-policy.md) | Native SDK payload policy — redistribute ZWO (MIT), download QHY firmware on-target (proprietary) |
| [ADR-014](decisions/014-zwo-per-device-services-and-link-features.md) | One service per ZWO device (EFW = future `zwo-filterwheel`); per-device SDK link features in `libzwo-sys`/`zwo-rs`; each zwo package ships only its own blob |
| [ADR-015](decisions/015-windows-packaging-architecture.md) | Windows packaging — one MSI suite, per-service Windows services; config/state are platform-dependent defaults in code, not installer artifacts |
| [ADR-016](decisions/016-service-config-ownership-and-doctor.md) | Service config ownership — installers place bytes, a standalone `rusty-photon-doctor` wires the configs; service facts only (device usage stays in `rp`); hardware checks split at the SDK line per ADR-014 |
| [ADR-018](decisions/018-svbony-sdk-no-license-payload-policy.md) | SVBony SDK payload policy — a third ADR-013 bucket for SDKs with no license grant at all: never redistribute, download-on-target like QHY |
| **Plans** (in-flight initiatives — see [docs/plans/](plans/)) | |
| [service-packaging.md](plans/service-packaging.md) | `.deb`/`.rpm` packages for every service (15 daemons — phd2-guider became one with its #464 HTTP service mode): shared `rusty-photon` user, hardened unit classes, QHY firmware downloader, ZWO blob bundling, on-rig arm64 builds. Behind [ADR-012](decisions/012-service-packaging-architecture.md)/[ADR-013](decisions/013-native-sdk-payload-policy.md) |
| [i18n.md](plans/i18n.md) | Workspace internationalization: scope, tech-stack, and translation-sourcing options |
| [zwo-driver.md](plans/zwo-driver.md) | ZWO ASI camera + EFW filter-wheel Alpaca driver (`zwo-camera`, port 11122) + author-maintained `zwo-rs`/`libzwo-sys` FFI; the ZWO analogue of `qhy-camera` (MIT SDK → public cache, but no pre-existing Rust FFI). See [`docs/services/zwo-camera.md`](services/zwo-camera.md) + [ADR-008](decisions/008-zwo-camera-native-sdk-ffi.md) |

Completed plans move to [`docs/plans/archive/`](plans/archive/) and are no longer
listed here.

## Shared Crates

| Crate | Location | Purpose |
|-------|----------|---------|
| [bdd-infra](../crates/bdd-infra/) | `crates/bdd-infra` | Shared BDD test infrastructure: `ServiceHandle` for spawning, managing, and stopping service binaries. The binary is located from the caller's package name (`env!("CARGO_PKG_NAME")`) via the conventional `{PACKAGE_UPPER_SNAKE}_BINARY` env override, else the Cargo / llvm-cov target dir (`$CARGO_TARGET_DIR` / `$CARGO_LLVM_COV_TARGET_DIR`, target-triple-aware), else by walking up for `target/debug/<pkg>`. See [testing.md](skills/testing.md) Section 5.1. |
| [rusty-photon-tls](../crates/rusty-photon-tls/) | `crates/rusty-photon-tls` | Opt-in TLS serving for inter-service communication: dual-stack TCP binding, TLS/plain serving, client CA trust, and the shared `TlsConfig` type. Certificate *provisioning* (self-signed issuance, ACME, DNS-01) lives in `services/doctor` (`doctor tls issue`; see [doctor.md](services/doctor.md)). See [ADR-002](decisions/002-tls-for-inter-service-communication.md). |
| [rp-auth](../crates/rp-auth/) | `crates/rp-auth` | Opt-in HTTP Basic Auth: Argon2id credential hashing/verification, axum tower middleware, and config types. See [ADR-003](decisions/003-authentication-for-device-access.md). |
| [rp-ephemeris](../crates/rp-ephemeris/) | `crates/rp-ephemeris` | Astronomical math: `Ephemeris` trait + `ErfarsEphemeris` impl wrapping the `erfars` ERFA bindings (BSD-licensed clean-room derivative of IAU SOFA). Pure functions for sidereal time, alt/az, transit, rise/set, twilight, sun + moon position. See [`docs/crates/rp-ephemeris.md`](crates/rp-ephemeris.md) for the crate design (panic safety, NaN-degradation, time scales); [`rp-planning-tools.md`](plans/archive/rp-planning-tools.md) for the original implementation plan. |
| [rp-catalog](../crates/rp-catalog/) | `crates/rp-catalog` | Embedded Messier + NGC + IC catalog (~13k objects, openNGC source, CC-BY-SA-4.0 attribution). `Catalog::resolve(name)` does case- and whitespace-insensitive lookup with alias support. See [`rp-planning-tools.md`](plans/archive/rp-planning-tools.md). |
| [skywatcher-motor-protocol](../crates/skywatcher-motor-protocol/) | `crates/skywatcher-motor-protocol` | Pure codec for the Sky-Watcher motor-controller wire protocol (USB + UDP/11880). Transport-agnostic; isolates the 24-bit low-byte-first hex encoding and the `+0x800000` position bias. Used by `star-adventurer-gti`. See [`docs/references/skywatcher-motor-controller-command-set.md`](references/skywatcher-motor-controller-command-set.md). |
| [rusty-photon-i18n](../crates/rusty-photon-i18n/) | `crates/rusty-photon-i18n` | Fluent loader + locale resolver shared across services. Reads `RP_LOCALE` / `LC_ALL` / `LC_MESSAGES` / `LANG` / OS, negotiates against the locales each consumer embeds, falls back to `en`. Owns `LocalizedParser` trait, `init` lifecycle, and an `ACTIVE_LOADER` thread-local for `value_parser` callbacks. First consumer: `ppba-driver` (CLI help + errors). See [`i18n.md`](plans/i18n.md) and [`i18n-cli-spike.md`](plans/archive/i18n-cli-spike.md). |
| [rusty-photon-i18n-derive](../crates/rusty-photon-i18n-derive/) | `crates/rusty-photon-i18n-derive` | Companion proc-macro crate. `#[derive(LocalizedParser)]` reads `#[localized(about = "key")]` / `#[localized(help = "key")]` attributes alongside `#[derive(Parser)]` and emits a `parse_localized(loader)` impl that mutates the clap `Command` before parse. Re-exported via `rusty_photon_i18n::LocalizedParser`. |
| [rusty-photon-shared-transport](../crates/rusty-photon-shared-transport/) | `crates/rusty-photon-shared-transport` | Refcounted multi-client lifecycle scaffolding for duplex transports (serial + UDP): `SharedTransport<Codec>`, the `TransportFactory` trait, and background polling. Basis of the shared-transport driver pattern (first adopter: `dsd-fp2`). |
| [rusty-photon-camera-core](../crates/rusty-photon-camera-core/) | `crates/rusty-photon-camera-core` | The vendor-neutral half of the three ASCOM camera drivers: ROI validation and its rule order (R2/R3), the bin-ratio ROI rescale (B3), binned-full-frame sensor alignment (R4), `BayerOffsetX/Y` from a canonical mosaic (ST1), the single-plane `ImageArray` unpack, and `PercentCompleted`'s cap. Two tests decide what belongs here, both about the *driver* half rather than about dependencies: nothing there implements ASCOM's `Camera`/`Device` traits or holds device state, and no vendor SDK type appears in a signature. ASCOM Alpaca is the workspace's lingua franca, so the crate speaks it (`ImageArray`, `ASCOMError`) rather than handing each driver a private dialect to translate — which is why each driver still maps its own SDK's Bayer spelling and readout formats onto the shared vocabulary. Used by `qhy-camera`, `zwo-camera`, `svbony-camera`. |
| [rusty-photon-driver](../crates/rusty-photon-driver/) | `crates/rusty-photon-driver` | Shared ASCOM-driver runtime layer: the common `DriverError` model, its ASCOM error-code mapping, and the generic `config.get`/`apply`/`schema` action dispatch. See [ADR-007](decisions/007-rusty-photon-driver-shared-crate.md). |
| [rusty-photon-config](../crates/rusty-photon-config/) | `crates/rusty-photon-config` | Shared config-path resolution, first-run `UniqueID` materialization, and the `config.get`/`apply`/`schema` action protocol for rusty-photon drivers. See [config-actions.md](services/config-actions.md). |
| [rusty-photon-service-lifecycle](../crates/rusty-photon-service-lifecycle/) | `crates/rusty-photon-service-lifecycle` | Unified service lifecycle: tokio runtime + signal handlers + optional Windows SCM, exposing a single `Shutdown` handle across the workspace. See [`docs/crates/rusty-photon-service-lifecycle.md`](crates/rusty-photon-service-lifecycle.md). |
| [rp-fits](../crates/rp-fits/) | `crates/rp-fits` | FITS reader/writer wrapper (pure-Rust `fitsrs`) for Rusty Photon services. See [ADR-001](decisions/001-fits-file-support.md). |
| [rp-plate-solver](../crates/rp-plate-solver/) | `crates/rp-plate-solver` | HTTP client for the `plate-solver` rp-managed service, used by `rp`'s `plate_solve` MCP tool. See [ADR-005](decisions/005-plate-solver.md). |
| [rp-guider](../crates/rp-guider/) | `crates/rp-guider` | HTTP client for the guider rp-managed service (`phd2-guider serve`), used by `rp`'s guiding MCP tools and the safety enforcer's stop-guiding-on-unsafe step. |
| [qhyccd-rs](../crates/qhyccd-rs/) | `crates/qhyccd-rs` (+ nested `libqhyccd-sys`) | Vendored first-party safe bindings for the proprietary QHYCCD SDK; `libqhyccd-sys` holds the raw FFI. Used by `qhy-camera`. See [ADR-009](decisions/009-vendor-qhyccd-rs.md). |
| [zwo-rs](../crates/zwo-rs/) | `crates/zwo-rs` (+ nested `libzwo-sys`) | Vendored first-party safe bindings for the ZWO ASI camera + EFW filter-wheel + EAF focuser SDK (MIT); `libzwo-sys` holds the raw FFI. Used by `zwo-camera` and `zwo-focuser`. See [ADR-008](decisions/008-zwo-camera-native-sdk-ffi.md) + [ADR-010](decisions/010-vendor-zwo-rs.md). |
| [svbony-rs](../crates/svbony-rs/) | `crates/svbony-rs` (+ nested `libsvbony-sys`) | Vendored first-party safe bindings for the SVBony camera SDK. Unlike `libzwo-sys`, `libsvbony-sys` is **hand-written, not `bindgen`-generated** — SVBony's SDK header carries no license text, so it is not vendored (mirrors `libqhyccd-sys`'s posture toward QHY's similarly unlicensed header). Video-only exposure model (no snap API); `simulation` feature models the soft-trigger video-capture flow + a poll-based cooling ramp. Phase A/B landed 2026-07-21; consumed by `services/svbony-camera` as a direct path dependency (not promoted to `[workspace.dependencies]` — Rule 10's promotion threshold is a second consumer) since Phase C — see [svbony-camera.md](plans/archive/svbony-camera.md). |

## Inter-Service Communication: MCP via `rmcp`

`rp` communicates with orchestrator plugins (e.g., `calibrator-flats`) using the
[Model Context Protocol](https://modelcontextprotocol.io/) (MCP). MCP was chosen
so that both the server (`rp`) and clients (plugins) can use standard,
well-maintained crates instead of hand-rolling JSON-RPC.

The workspace uses [`rmcp`](https://crates.io/crates/rmcp) (the official MCP Rust
SDK from the modelcontextprotocol org). Key reasons for choosing `rmcp`:

- **Official SDK** — maintained by the modelcontextprotocol org, tracks spec
  changes first
- **Both roles, one crate** — `"server"` and `"client"` feature flags on the
  same crate, sharing types
- **Composable HTTP** — `StreamableHttpService` implements Tower `Service`, so
  it mounts on `rp`'s existing axum router via
  `Router::nest_service("/mcp", ...)`
- **Dependency alignment** — uses axum 0.8 and reqwest 0.13, matching the
  workspace
- **Ergonomic tool definitions** — `#[tool]` derive macro on impl methods

Workspace dependency (in root `Cargo.toml`):
```toml
rmcp = { version = "1.7", default-features = false }
```

Service feature selections:
- `rp`: `features = ["server", "macros", "transport-streamable-http-server", "schemars"]`
- `calibrator-flats`: `features = ["client", "transport-streamable-http-client-reqwest"]`

`schemars` 1.0 is also a workspace dependency — rmcp's `#[tool]` macro
generates JSON Schema from parameter structs via `schemars::JsonSchema`.

## Shared Architecture Patterns

### Serial-based services (ppba-driver, qhy-focuser)

```
config.rs         — Configuration types and JSON loading
config_actions.rs — `config.get` / `config.apply` / `config.schema` action handlers
error.rs          — Service-specific error enum (thiserror)
serial.rs         — tokio-serial-backed `TransportFactory` (wraps the port in a `SerialFrameTransport`)
codec.rs          — `Codec` adapter: device wire frames ⇄ `SharedTransport`
mock.rs           — In-memory mock `TransportFactory` (cfg(feature = "mock"))
protocol.rs       — Wire-format encode/decode for the device's serial protocol
manager.rs        — Thin wrapper over `rusty_photon_shared_transport::SharedTransport` (refcounted connect + background polling + cached state)
*_device.rs       — ASCOM trait implementation
lib.rs            — ServerBuilder (CLI args → server)
main.rs           — Entry point
```

The legacy per-service `io.rs` traits and `serial_manager.rs` are gone — the
refcounted connection lifecycle and the `TransportFactory` / `Codec` traits now
live in the
[`rusty-photon-shared-transport`](../crates/rusty-photon-shared-transport/)
crate; each service keeps only its handshake, poll body, and cached state.

ppba-driver additionally has `switches.rs` (Switch device wiring) and
`mean.rs` (running-mean smoothing for ObservingConditions readings); its device
files are `observingconditions_device.rs` + `switch_device.rs`.

### HTTP gateway services (rp)

```
config/              — Configuration types + loading (camera/mount/focuser/site/… submodules)
error.rs             — RpError enum + Result alias (thiserror)
equipment/           — EquipmentRegistry + ASCOM Alpaca client (per-device submodules)
events.rs            — EventBus, webhook + SSE delivery
imaging/             — FITS read/write, pixel statistics, analysis + tools
mcp/                 — rmcp tool_router: #[tool] methods, ServerHandler impl
persistence/         — redb document store + FITS cache (cache/document/fits)
planner/             — Observation planning (catalog/decision/primitives/convenience)
session.rs           — SessionManager, orchestrator invocation
routes.rs            — Axum router (REST + MCP + SSE endpoints)
lib.rs               — ServerBuilder (two-phase: build → start)
main.rs              — Entry point
```

### Orchestrator plugins (calibrator-flats)

Plugins act as MCP clients of `rp` and expose an HTTP `/invoke` endpoint that
`rp` calls when a session is started.

```
config.rs    — Plugin config + FlatPlan request schema
error.rs     — CalibratorFlatsError enum
mcp_client.rs — rmcp StreamableHttpClient wrapper for calling rp's tools
workflow.rs  — Iterative exposure optimization + batch capture state machine
routes.rs    — Axum router: GET /health, POST /invoke
lib.rs       — Plugin server bootstrap
main.rs      — Entry point
```

### Monitoring service (sentinel)

`sentinel` is a standalone Axum + reqwest backend. The dashboard at
`http://127.0.0.1:11114/` is hand-rolled HTML built with `format!()` in
`services/sentinel/src/dashboard.rs`, refreshed client-side by a vanilla
`fetch()` loop hitting `/api/status` and `/api/history` every five seconds.

```
sentinel/src/
  config.rs        — Config types: monitors, notifiers, dashboard
  error.rs         — SentinelError enum
  io.rs            — HTTP client trait abstraction (testability)
  alpaca_client.rs — ASCOM Alpaca SafetyMonitor client
  monitor.rs       — Monitor trait + state types
  pushover.rs      — Pushover notifier
  notifier.rs      — Notifier trait
  state.rs         — Shared monitor status + notification history
  engine.rs        — Orchestrates monitors, transitions, notifiers
  watchdog.rs      — Operation watchdog (predictive-deadlines Phase 4)
  corrective.rs    — Corrective-action ladder (predictive-deadlines Phase 5)
  dashboard.rs     — Axum routes for JSON API + dashboard HTML
  lib.rs / main.rs — Server bootstrap and entry point
```

> A `sentinel-app` Leptos/WASM crate was scaffolded as an alternative
> dashboard frontend and later abandoned in favour of the hand-rolled UI
> above (and the `ui-htmx` direction for config UIs). It was removed in
> 2026-06; see
> [docs/plans/archive/sentinel-app-leptos-dashboard.md](plans/archive/sentinel-app-leptos-dashboard.md).

## MSRV

The minimum supported Rust version is pinned in `[workspace.package]` of the
root `Cargo.toml` (`rust-version = "1.94.1"`). Every member listed in
`[workspace].members` — all services and shared crates — inherits it via
`rust-version.workspace = true`.

## Supported targets

**Little-endian only.** The catalog format (`rp-catalog`), the mount wire
protocols (`skywatcher-motor-protocol`, `pa-falcon-rotator`) and the camera SDK
buffers all read and write native integers without byte-swapping, and none of it
has ever been built or validated big-endian. A BE build would decode a star
catalog or an encoder position with the bytes reversed and report no error at
all, so the workspace refuses to compile there:

```rust
// crates/rusty-photon-service-lifecycle/src/lib.rs
#[cfg(not(target_endian = "little"))]
compile_error!("Rusty Photon supports little-endian targets only. ...");
```

Cargo.toml has no way to express a target guard, so it lives in the root of the
crate every binary depends on — 44 of the 49 workspace members, and every
service. Verify it with a cross-check against an installed BE target:

```sh
cargo check -p rusty-photon-service-lifecycle --target s390x-unknown-linux-gnu
```

The shipped targets are x86-64 and aarch64 on Linux, macOS and Windows, all
little-endian. Lifting this restriction means auditing every decoder first, not
deleting the guard.

## Workspace Dependencies

Dependencies used by two or more services are declared in the workspace
`Cargo.toml` under `[workspace.dependencies]` (CLAUDE.md Rule 10). Services
reference them with `dep.workspace = true`.

### Dual-homed crates inherit shared deps too

The dual-homed members (`zwo-rs` + `libzwo-sys`, `qhyccd-rs` + `libqhyccd-sys` —
ADR-009/010) follow the same rule: their **shared** third-party dependencies
(e.g. `thiserror`, `tracing`, and the simulation-only `rand`/`rayon` shared
between the two camera crates) inherit from `[workspace.dependencies]` with
`dep.workspace = true`. This is safe for their independent crates.io releases
because `cargo publish` **flattens** an inherited dependency into a concrete
version in the packaged manifest (verified by dry-run). What stays explicit on
these members is their **package identity metadata** (`version` / `edition` /
`license` / `authors` / `description` / `keywords` / `categories`) — *not*
`*.workspace = true` — so they release on their own cadence (the carve-out
recorded in ADR-009/010). A dep is left crate-local only when it is genuinely
single-consumer (e.g. `libzwo-sys`'s `bindgen` build-dep) or when the workspace
pin would force an unwanted feature (e.g. `qhyccd-rs` keeps `tracing-subscriber`
local to avoid the workspace's `env-filter`).

### Duplicate transitive versions

Dependabot bumps one package at a time, so `Cargo.lock` accumulates several
versions of the same transitive crate. To list them:

```sh
cargo tree --workspace --target all -d
```

`--target all` matters — a large part of the split is Windows-only and is
invisible from a Linux host.

A duplicate is only *resolvable* when every consumer's requirement admits one
common version, which in practice means some consumer carries an open range
(`windows-sys = ">=0.52, <0.62"`). Duplicates whose consumers pin incompatible
majors (`^0.22` vs `^0.23`) cannot be collapsed by a lock refresh at all — they
need an upstream release or a dependency swap. Most of this workspace's
duplicates are that second kind: either an ecosystem mid-migration (`rand`
0.8/0.9/0.10, `syn` 2/3, `thiserror` 1/2, `hashbrown`, `getrandom`) or a single
crate holding an old major open (`serialport` → `nix 0.26` + `windows-sys 0.52`,
`ring` → `windows-sys 0.52`, `system-configuration` → `core-foundation 0.9`).
`cargo tree` is the authority on which consumers
actually pull a given version:

```sh
cargo tree --workspace --target all -i windows-sys@0.52.0 --depth 1
```

To see the requirements behind that, read the `req` of each dependent — but
filter the edges Cargo actually resolves, or the answer is wrong:

```sh
cargo metadata --format-version 1 --all-features | \
  jq -r '.packages[] | . as $p | .dependencies[] |
         select(.name=="windows-sys" and .kind==null and .optional==false) |
         "\(.req)  <- \($p.name) \($p.version)"' | sort -u
```

`.kind` is `"dev"`, `"build"`, or null for a normal dependency. Cargo does not
resolve dev-dependencies of non-workspace packages, and an unactivated
`optional` dependency constrains nothing either — so without both filters the
recipe reports crates that hold nothing as blockers.

Chasing a duplicate is not free. Forcing an open-range consumer onto a different
version with `cargo update -p <crate> --recursive` can move *other* crates
**down** onto an older shared version, which is worse than the split it was
meant to fix. Take the refresh only when `cargo tree --workspace --target all -d`
shows the version count actually dropping.

### Holding a transitive dependency back

A `=x.y.z` requirement in `[workspace.dependencies]` constrains only the crates
a workspace member names directly. To hold a **transitive** crate, pin it in
`Cargo.lock`:

```sh
# Order matters: rust-embed-impl 8.12 requires rust-embed-utils ^8.12, so
# pinning utils first aborts with a resolver error. Pin impl, then utils.
cargo update -p rust-embed-impl  --precise 8.11.0
cargo update -p rust-embed-utils --precise 8.11.0
```

Pinning only one of a lockstep pair is not enough either: with `impl` alone held,
`utils` stays on 8.12 and still drags the `digest 0.11` chain in. Verify with
`cargo tree --workspace --target all -i digest` showing a single version.

A plain `cargo update` undoes all of it, so re-apply the pins (and re-run the
Bazel repin from CLAUDE.md rule 10) whenever the lock is refreshed. Current
holds:

| Crate | Held at | Why |
|---|---|---|
| `rust-embed`, `rust-embed-impl`, `rust-embed-utils` | 8.11.0 | `rust-embed` embeds the Fluent translation assets into ppba-driver's binary. From 8.12, `rust-embed-utils` pulls `sha2`/`digest 0.11` in beside the `digest 0.10` chain `argon2`/`blake2` already bring. ppba-driver is the only service with a direct `rust-embed` dependency, so it is the only package that gets the duplicate — and its two largest test targets then fail `rustc` E0463 "can't find crate" under `bazel / windows-latest`, deterministically. The duplicate graph is the confirmed cause: bumping `rust-embed` to 8.12 together with `argon2` 0.6 / `blake2` 0.11, which puts the whole graph on `digest 0.11`, builds and tests green on Windows. **Lift the hold when `argon2` 0.6 and `blake2` 0.11 go stable** — both are release-candidate only today — and bump all three together. |

### Pre-commit hooks

The workspace uses `cargo-husky` as a dev-dependency configured with
`default-features = false` and the `precommit-hook` + `user-hooks` features
(see root `Cargo.toml`). The `user-hooks` feature tells `cargo-husky` to
install a custom hook script kept in the repo at
`.cargo-husky/hooks/pre-commit`, which currently runs:

```sh
cargo clippy --all --all-targets --all-features -- -D warnings
cargo clippy --all --lib --bins -- -D warnings   # default-features pass (#988)
cargo fmt --all -- --check
# Buildifier (BUILD / *.bzl / MODULE.bazel formatting + lint) — the same gate CI
# runs. Guarded on bazel being installed, so Cargo-only devs aren't blocked:
bazel test //:buildifier_check
```

The hook is installed automatically the first time any test build pulls
`cargo-husky` in as a dev-dependency.

## Coding Conventions

### Lints

`[workspace.lints.clippy]` in the root `Cargo.toml` denies the routes by which
an otherwise-recoverable situation aborts the process — `unwrap_used`,
`expect_used`, `unreachable`, `panic`, `todo`, `unimplemented`,
`panic_in_result_fn`, `unchecked_time_subtraction`, `string_slice`. A driver
that panics at 2am ends the night's imaging (tenet 2). On top of the per-lint
denies, the `pedantic` and `nursery` groups are denied wholesale (at
`priority = -1`, so per-lint entries outrank the group level), zeroed
rung by rung by the L6b ladder before the flip
([docs/plans/archive/workspace-lints.md](plans/archive/workspace-lints.md)). Members opt in with
`[lints] workspace = true`; the dual-homed FFI crates (`qhyccd-rs`, `zwo-rs`,
`svbony-rs` and their `-sys` shims) instead adopt a **concrete, verbatim copy**
of the workspace table, family by family as the L7 ladder zeroes their sites
(all three families carry it as of the qhy rung). Inheritance cannot resolve in the
publish-readiness check's out-of-tree copied build, and the copy changes
nothing for consumers — `cargo package` inlines `workspace = true` lints
anyway, so copy and inheritance publish identical artifacts, and registry
dependencies build under `--cap-lints allow` regardless. The copies are held
in lockstep with the root table by `tools/ci/check_lints_parity.py`, a step of
the required `stable / clippy` gate. Before its rung a family carried either
no `[lints]` table or a deliberate partial mirror (qhyccd-rs kept a
standalone `[lints.rust]` `unexpected_cfgs` entry so the crate stayed
publishable out-of-tree); the guard still holds any such partial table to the
workspace text at whole-tool granularity.

`exit` is deliberately **not** denied: every call site is a `doctor.rs`
`pub fn run(...) -> !` honouring doctor's documented 0/1/2 exit contract
(see [doctor](services/doctor.md)), so denying it would buy only `#[allow]`s.

Test code is exempt where the panic is the point, and that exemption lives in
`clippy.toml` at the repo root rather than in per-module attributes. It turns
`unwrap_used`, `expect_used`, `panic` and `indexing_slicing` off **in test
scope only**, so a `#[cfg(test)] mod tests { ... }` needs no `#[allow]` of its
own and production code in the same file still gets the full deny.

You still need a scoped attribute in three cases:

- **Lints with no knob** — clippy offers `allow-*-in-tests` for only eight
  lints; `unreachable`, `string_slice`, `arithmetic_side_effects` and
  `as_conversions` are not among them. For the knobless lints inside
  `#[cfg(test)]` mods in `src/`, the crate root carries
  `#![cfg_attr(test, allow(...))]`: it is active only in the test-profile
  compilation, so every production line is still linted in the non-test
  compilation of the same target. Feature-gated mock modules are **not**
  exempt — they are ordinary lib code whose outputs tests assert against,
  and are held to production standard.
- **Cucumber step definitions.** Clippy treats `#[cfg(test)]` modules and
  `#[test]` functions as test code, but `#[given]`/`#[when]`/`#[then]` are
  plain functions in a test crate. Every file directly under `tests/` is its
  own crate root and therefore carries its own file-level `#![allow(...)]`,
  which submodules pulled in via `mod`/`#[path] mod` inherit — a service with
  both `tests/bdd.rs` and `tests/test_lib.rs` needs the attribute on each.
- **Panics inside closures and helper fns in a test crate.** The knobs
  recognise the `#[test]` function itself, not a closure it builds or a
  `tests/common/mod.rs` helper it calls.

Before deleting an `#[allow]`, resolve its **scope** — a per-file assumption is
wrong. An inner `#![allow]` in a file's header region covers the whole package,
and an outer `#[allow]` on a `mod name;` declaration covers that module's file
subtree. [docs/plans/archive/workspace-lints.md](plans/archive/workspace-lints.md)
records the ladder that widened this set.

**Stable gates, beta reports.** `check.yml` runs clippy on both channels with
deliberately different policies: `stable / clippy` is the required PR gate at
`-D warnings` — two passes, `--all-features --all-targets` plus a
default-features `--lib --bins` pass, because each compiles feature-gated code
the other cfgs out (#988; the second is `--lib --bins` so dev-dependency edges
cannot force `simulation`/`mock` back on via feature unification) — while
the nightly `beta / clippy` job passes `--cap-lints warn`
so it can never fail on a lint — not even one added upstream to a group this
workspace denies. Its findings become one `beta-clippy`-labeled issue per lint,
which closes itself once the lint stops firing. Widening the deny set here
therefore costs nothing in nightly red; see
[docs/skills/pre-push.md](skills/pre-push.md#checkyml).

### Duration Units

**Durations are `std::time::Duration` system-wide.** Any field, parameter,
return value, or struct member that represents a time interval uses
`Duration` end-to-end — config, internal state, MCP tool parameters,
inter-service wire payloads, and (where types allow) telemetry. Integer
representations of duration (`u32 ms`, `u64 ms`, `u64 secs`) do **not**
appear in internal data structures; they exist only as transient values
at boundaries that demand them (third-party SDKs, JSON-RPC payloads
with a fixed wire schema, sentinel/dashboard JSON serialisation of
already-elapsed magnitudes).

**Construct with the coarsest constructor that fits.** `Duration::from_mins(2)`
and `Duration::from_hours(1)`, not `from_secs(120)` and `from_secs(3600)` —
clippy's `duration_suboptimal_units` enforces this now that `nursery` is denied, and
the coarse form is the one a reader can check at a glance. Both are stable since
Rust 1.91, under the declared MSRV.

**Precision floor: microseconds.** The system-wide precision contract
is 1 µs. This is finer than what most observing workflows need but
matches the actual minimum exposure of modern CMOS sensors (QHY174
~50 µs, QHY600 ~10 µs, ZWO ASI line ~32 µs). It is required for **bias
frames**, which use the camera's true minimum exposure to capture the
read-noise floor — a 1 ms floor would expose 20–100× longer than the
sensor's minimum and accumulate dark current that contaminates the
bias. Sub-microsecond precision is not required: ASCOM Alpaca's
`Camera.StartExposure` Duration is an `f64` in seconds (so the
protocol can express it), but no current sensor honours it, and
QHY's nanosecond-resolution SDK API offers no observable advantage
at this precision.

For **config types** (anything deserialised from a JSON config file),
use `std::time::Duration` with the `humantime-serde` adapter and **no
unit suffix in the field name**:

```rust
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileConfig {
    pub path: PathBuf,
    #[serde(with = "humantime_serde", default = "default_polling_interval")]
    pub polling_interval: Duration,
}

fn default_polling_interval() -> Duration {
    Duration::from_secs(60)
}
```

The wire format is a humantime string (`"60s"`, `"500ms"`, `"50us"`,
`"1m30s"`, `"2h"`). The unit lives in the value, not the field name —
the type already says `Duration` and the value already says the unit.
This removes the previous `_ms` vs `_secs` ambiguity in field names.

`humantime` accepts both compact forms (`"5m"`) and combinations
(`"1m30s500ms"`). It rejects bare integers (`"30"` is invalid — must be
`"30s"` or `"30ms"`).

For raw integer fields that are still magnitudes of time but **not**
internal `Duration`s (e.g. dashboard JSON serialising an elapsed
magnitude, or a `u64` epoch millisecond timestamp), keep the unit
suffix on the field name (`last_poll_epoch_ms`, `elapsed_ms`) so a
reader can tell the unit at the call site.

**Boundary conversions.** When a `Duration` must be flattened to an
integer or string for a third-party wire format, do it at the boundary
only — never store the integer back into an internal struct. Use
`humantime::format_duration(d)` to render a `Duration` to a humantime
string preserving µs precision (instead of `format!("{}ms",
d.as_millis())`, which collapses sub-ms values to `"0ms"`). When the
external schema demands a bare integer (e.g. PHD2's `time` and
`timeout` settle keys), apply whatever rounding the wire format
requires at the `json!` site — `.as_micros()` / `.as_millis()` /
`.as_secs()` when truncation is acceptable, or a boundary helper such
as `settle_secs_ceil` when sub-second values must round up instead of
truncating to `0`. See `services/phd2-guider/src/client.rs` for the
worked example.

**Operator-facing durations.** Any duration string a human operator
reads — push notifications, UI display, doctor output, operator-read
log lines — is rendered with `humantime::format_duration`, never a
hand-rolled formatter or a numeric `format!("{:.1}s", …)` /
`{:?}`-on-a-`Duration`: an operator reads `5m`, not `300.0s`. Numeric
wire fields (`duration_secs`, `elapsed_ms`), `humantime_serde` config
fields, and internal timing arithmetic are unaffected.

### Enum derives: which crate to reach for

Four crates in this workspace can generate a `Display`. Pick by what the
string is derived *from*:

| Need | Reach for |
|---|---|
| An error type | `thiserror` — `#[error("…")]` |
| **Any** `Display` — a variant name, a per-variant literal, or a string interpolating runtime fields | `derive_more` — `#[display("…")]` |
| Variant iteration, count, or integer → variant | `strum` — `VariantArray`, `EnumCount`, `FromRepr` |
| An allocation-free `&'static str`, or a `Display` that must round-trip back through `FromStr` | `strum` — `IntoStaticStr`, `Display` + `EnumString` |
| A parse error a **human or an ASCOM client reads** | hand-written `FromStr` + a bespoke error |

`Display` alone is `derive_more`'s. A bare `#[derive(derive_more::Display)]`
on a fieldless enum already renders the variant name, so it covers the
plain case with no attributes at all, and it is the only one of the two
that can interpolate a field. Reach past it to `strum` only for something
`derive_more` cannot express:

- **`VariantArray` / `EnumCount`** have no `derive_more` counterpart, and
  they retire whole classes of drift: a hand-maintained `const ALL` array
  that silently omits a variant, and a hand-synced `COUNT` constant.
  `FromRepr` does have one — `derive_more`'s `TryFrom` with
  `#[try_from(repr)]` — which returns `Result` where strum returns
  `Option`; pick by which the call site wants.

**No derive in either crate converts a variant *to* an integer.** strum's
18 derives and `derive_more`'s 19 all go the other way or work on variant
fields; `num_enum::IntoPrimitive` is the crate that does it, and it is not
a workspace dependency. So a type that needs a stable numeric id should
carry it as data rather than lean on the discriminant — `SwitchId` keeps
its ASCOM ids in `SwitchInfo::id` and resolves `from_id` through that
table, which also frees the variants to be reordered.
- **`IntoStaticStr`** yields `&'static str`; `derive_more`'s only string
  derive allocates. `ConfigAction::name()` is `self.into()` because of it.
- **`Display` + `EnumString`** read the *same* `#[strum(serialize = "…")]`
  attribute, so a wire string is written once and provably round-trips.
  `derive_more`'s `Display` and `FromStr` are independent: `FromStr`
  matches variant *names*, case-insensitively, and ignores `#[display]`
  literals entirely. A type carrying `#[display("config.get")]` renders
  `config.get` but parses only `Get`, `get`, or `GET` — the literal and
  the accepted input drift apart with nothing to flag it. Any type whose
  string must survive a round trip belongs to `strum`.

A type that keeps `strum` for one of the above should take its `Display`
from `strum` too. Splitting one type's wire string across
`#[strum(serialize = …)]` and `#[display(…)]` creates two
independently-editable copies of the same protocol name.

**Never derive a string with a casing style.** `#[strum(serialize_all =
…)]` and `derive_more`'s `#[display(rename_all = …)]` are banned on any
type whose string crosses a compatibility boundary — a config-file
value, an Alpaca `Action` name, an on-disk key, a device wire token.
Pin every such string with an explicit per-variant literal —
`#[strum(serialize = "…")]` or `#[display("…")]` — which keeps it
greppable in the source and keeps an identifier rename a pure refactor.
Three converters are in play and they disagree: `strum` uses `heck`,
`derive_more` uses `convert_case`, and `serde`'s `rename_all` agrees
with `heck` — but only on some inputs. `ApPark0` snake-cases to
`ap_park0` under `heck` (not `ap_park_0`), and `Uuid8` is `uuid8` under
`heck` but `uuid_8` under `convert_case`. A type carrying both
`#[serde(rename_all)]` and a derive's own casing attribute has two
independent sources of truth that neither macro can cross-check.

Where a type's serde casing and its `Display` casing deliberately
differ — `MonitorState`, `ServiceHealth` — that split is intentional
and documented on the type. Do not unify them.

**Banned outright:**

- `#[strum(disabled)]` — makes `Display`/`AsRef`/`Into<&'static str>`
  panic at runtime, and clippy does not flag macro-expanded panics, so
  it clears the whole quality gate and fires in the field instead
  (tenet 2).
- `EnumProperty` — its lookup returns `Option` off a runtime `&str`
  key, and under the workspace's `unwrap_used` deny the only available
  fallback is a silent wrong value.
- Deriving both `VariantNames` and `VariantArray` on one type — makes
  `T::VARIANTS` ambiguous (E0034). Use `VariantArray`.

`strum` is pinned to the same minor `jsonschema` requires, so it adds
zero packages to the graph; see the comment on the dependency in the
workspace `Cargo.toml`.

`derive_more` enables `debug` and `display` at the workspace level, so a
member takes `derive_more = { workspace = true }` and needs no per-member
`features`. That is deliberate rather than lax: feature unification means
a whole-workspace `cargo` or `bazel` build resolves `display` through
*some* member regardless, so a missing per-member declaration used to
compile everywhere except `cargo build -p <member>` — which nothing but
the nightly `cargo hack --feature-powerset` job runs. Declaring it once
closes that green-PR/red-nightly gap at no cost: `display` adds only
`convert_case` and `unicode-segmentation`, both proc-macro-only.

## Feature Flags

- **`mock`** — Enables an in-memory mock factory with persistent device state
  for integration testing (ConformU, server tests); not used for unit tests,
  which define inline mocks. The serial drivers expose a per-service mock
  `TransportFactory` (`ppba-driver` → `MockPpbaTransportFactory`, `qhy-focuser`
  → `MockQhyTransportFactory`). Declared by `ppba-driver`, `qhy-focuser`,
  `pa-falcon-rotator`, `dsd-fp2`, `star-adventurer-gti`, `sky-survey-camera`
  (`mock = []`), the camera drivers `qhy-camera` / `zwo-camera`
  (`mock = ["simulation"]`), and `rp-plate-solver` / `rp-guider`
  (`mock = ["dep:mockall"]`).

## Build Notes

- The `ascom-alpaca` crate is a git dependency on upstream
  `RReverser/ascom-alpaca-rs.git` (branch `main`, `default-features = false`).
  All of our PRs against upstream have merged, so the `ivonnyssen` fork (and its
  `integration` / `pr/integer-parameter-handling` branches) is retired. Once
  upstream publishes a crates.io release containing these changes, switch to a
  versioned dependency.
- `.cargo/config.toml` sets `AWS_LC_SYS_USE_SYSTEM=0` for every Cargo build.
  Left unset, `aws-lc-sys`'s build script probes `OPENSSL_DIR` and then
  pkg-config for a system AWS-LC and links it dynamically when it finds one —
  which would give the shipped `.deb`/`.rpm` (built by
  `scripts/build-packages.sh` with a plain `cargo build --release`) a runtime
  dependency the field rig does not have. Bazel does not read that file, so the
  same pin is applied there through the `aws-lc-sys` `crate.annotation` in
  `MODULE.bazel`, which additionally keeps the build-script action hermetic:
  none of the probed host state is part of its action key, so an unpinned build
  on a host that has AWS-LC installed would publish a dynamically-linked
  artifact into the shared disk and remote caches.

### Bazel

Bazel is the per-PR build / test / coverage gate. `Cargo.toml` and `Cargo.lock`
remain the single source of truth for dependency versions, and Bazel's
`crate_universe` reads them. The repo root holds `MODULE.bazel` and `BUILD.bazel`;
`bazel test //...` runs all non-`requires-cargo`, non-BDD targets, and
`bazel test --test_tag_filters=bdd //...` adds the BDD suites. The required PR
checks are `bazel / <os>` (build + test on Linux/macOS/Windows), `bazel coverage`,
plus the Cargo `stable / fmt` and `stable / clippy` lint jobs (Bazel does not run
rustfmt/clippy). `bazel/cargo target parity` and the Cargo build/test jobs run
nightly as a safety net (coverage is Bazel-only), as do the `windows / clippy` +
`macos / clippy` legs that enforce the clippy deny set on OS-cfg'd code the
ubuntu gate never compiles. `bazel build //... && bazel test //...` is
the local pre-commit loop (see [docs/skills/pre-push.md](skills/pre-push.md)).

After adding a crates.io dependency to the workspace, run
`CARGO_BAZEL_REPIN=1 bazel mod tidy && bazel mod tidy` to refresh
`MODULE.bazel.lock` before committing. The second, un-forced `bazel mod tidy`
resets the lock's recorded `CARGO_BAZEL_REPIN` env fingerprint to `null` so the
committed lock doesn't churn on later plain `bazel` runs.

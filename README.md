# rusty_photon [![Build Status](https://github.com/rusty-photon/rusty-photon/workflows/bazel/badge.svg)](https://github.com/rusty-photon/rusty-photon/actions) [![Coverage Status](https://coveralls.io/repos/github/rusty-photon/rusty-photon/badge.svg?branch=main)](https://coveralls.io/github/rusty-photon/rusty-photon?branch=main) [![Dependency status](https://deps.rs/repo/github/rusty-photon/rusty-photon/status.svg)](https://deps.rs/repo/github/rusty-photon/rusty-photon) [![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

Cross-platform [ASCOM Alpaca](https://www.ascom-alpaca.org/) services and tools for observatory automation. ASCOM Alpaca is an open HTTP/REST standard for controlling astronomy equipment — these services expose real hardware as network-accessible devices that any Alpaca-compatible client (NINA, SGPro, Voyager, etc.) can discover and control.

**Platforms:** Linux, macOS, Windows (all services). Designed to run efficiently on hardware as small as a Raspberry Pi 5.

## Services

Coverage comes from the `bazel coverage` job (`.github/workflows/bazel-coverage.yml`), uploaded to [Coveralls](https://coveralls.io/github/rusty-photon/rusty-photon?branch=main), which drives the badge above and renders per-file line-level coverage. It is the sole coverage source and a required per-PR gate; the nightly Cargo jobs do not collect coverage.

| Service | Type | Port | Description |
|---------|------|------|-------------|
| [rp](services/rp) | Equipment gateway | 11115 | Main application: MCP tools, event bus, safety enforcer |
| [filemonitor](services/filemonitor) | ASCOM SafetyMonitor | 11111 | Monitors file content for observatory safety status |
| [ppba-driver](services/ppba-driver) | ASCOM Switch + ObservingConditions | 11112 | Driver for Pegasus Astro Pocket Powerbox Advance Gen2 |
| [qhy-focuser](services/qhy-focuser) | ASCOM Focuser | 11113 | Driver for QHY Q-Focuser (EAF) |
| [phd2-guider](services/phd2-guider) | Client library | — | Rust client for PHD2 autoguiding via JSON RPC |
| [sentinel](services/sentinel) | Monitoring service | 11114 | Polls devices, sends notifications, serves web dashboard |
| [calibrator-flats](services/calibrator-flats) | Tool provider (MCP server aggregated by rp; MCP client of rp) | 11170 | `train_flats` / `take_flats` / `get_flat_training` per optical train, flat timing remembered in a redb store |
| [polar-align](services/polar-align) | Orchestrator (MCP client of rp) | 11172 | Plate-solving polar alignment orchestrator for equatorial mounts |
| [sky-survey-camera](services/sky-survey-camera) | ASCOM Camera (simulator) | 11116 | Camera simulator that returns NASA SkyView cutouts for the configured optics |
| [star-adventurer-gti](services/star-adventurer-gti) | ASCOM Telescope | 11117 | Driver for Sky-Watcher Star Adventurer GTi (USB and WiFi/UDP) |
| [pa-falcon-rotator](services/pa-falcon-rotator) | ASCOM Rotator + Switch (status) | 11118 | Driver for Pegasus Astro Falcon Rotator (firmware ≥ 1.3) |
| [pa-scops-oag](services/pa-scops-oag) | ASCOM Focuser | 11123 | Driver for Pegasus Astro Scops OAG (motorized off-axis guider focuser) |
| [dsd-fp2](services/dsd-fp2) | ASCOM CoverCalibrator | 11119 | Driver for Deep Sky Dad Flat Panel 2 (motorised flat field panel) |
| [ui-htmx](services/ui-htmx) | Web config UI (BFF) | 11120 | Server-rendered configuration UI (axum + Maud + HTMX); edits any driver's config via its `config.get`/`config.apply` actions |
| [plate-solver](services/plate-solver) | rp-managed HTTP service | 11131 | Wraps the ASTAP CLI for plate solving in a supervised, crash-isolated process |
| [qhy-camera](services/qhy-camera) | ASCOM Camera (+ FilterWheel) | 11121 | Driver for QHYCCD cameras + filter wheels (vendored `qhyccd-rs` bindings; links the proprietary SDK unless `QHYCCD_SKIP_NATIVE_LINK=1`) |
| [zwo-camera](services/zwo-camera) | ASCOM Camera | 11122 | Driver for ZWO ASI cameras (vendored `zwo-rs` bindings, MIT SDK; links only the camera SDK — ADR-014 — unless `ZWO_SKIP_NATIVE_LINK=1`); the EFW filter wheel is a future separate service |
| [zwo-focuser](services/zwo-focuser) | ASCOM Focuser | 11124 | Driver for the ZWO EAF (vendored `zwo-rs` bindings, MIT SDK; links only the focuser SDK — ADR-014 — unless `ZWO_SKIP_NATIVE_LINK=1`) |
| [planetarium-bridge](services/planetarium-bridge) | ASCOM Telescope (virtual) | 11126 | Virtual target-entry telescope for planetarium apps (SkySafari etc.): Align imports the selection as a paused rp target; never touches hardware |
| [doctor](services/doctor) | Install diagnosis CLI | — | Read-only diagnosis of a multi-service install: config parsing, port collisions, cross-service wiring, unit and privilege gaps (ADR-016) |

### RP (Main Application)

Equipment gateway, event bus, and safety enforcer. Exposes all hardware as MCP tools, emits events for plugins to consume, and enforces safety constraints. Orchestration is handled by separate orchestrators (`session-runner`, `polar-align`) that start their own runs and drive the session by calling tools on `rp`; `rp` registers and supervises none of them. Tool providers (`calibrator-flats`) extend `rp`'s catalog instead: `rp` dials them at startup and proxies their tools.

See [docs/services/rp.md](docs/services/rp.md) for design documentation.

### Filemonitor

ASCOM Alpaca SafetyMonitor that reads a plain text file and evaluates configurable regex/contains rules to determine observatory safety status. Supports case-sensitive and case-insensitive matching with per-rule safe/unsafe outcomes.

See [docs/services/filemonitor.md](docs/services/filemonitor.md) for design documentation.

### PPBA Driver

ASCOM Alpaca Switch and ObservingConditions driver for the Pegasus Astro Pocket Powerbox Advance Gen2. Exposes 16 switches (6 controllable power/dew/USB outputs, 10 read-only sensors) over serial. Includes dynamic write protection for dew heaters when auto-dew is enabled.

See [docs/services/ppba-driver.md](docs/services/ppba-driver.md) for design documentation.

### QHY Focuser

ASCOM Alpaca Focuser driver for the QHY Q-Focuser (Electronic Auto Focuser). Communicates via a JSON-based command/response protocol over USB-CDC serial. Supports absolute and relative moves, speed configuration, temperature readout, and motor hold current settings.

See [docs/services/qhy-focuser.md](docs/services/qhy-focuser.md) for design documentation.

### PHD2 Guider

Rust client library for programmatic control of [PHD2](https://openphdguiding.org/) autoguiding. Provides JSON RPC 2.0 communication, event subscription, guiding control (start, stop, dither, pause), calibration, camera control, profile management, and auto-reconnect logic. Includes a `mock_phd2` binary for testing without hardware.

See [docs/services/phd2-guider.md](docs/services/phd2-guider.md) for design documentation.

### Sentinel

Observatory monitoring and notification service. Polls ASCOM Alpaca SafetyMonitor devices, detects safe/unsafe state transitions, sends push notifications via Pushover, and serves a live web dashboard. Unlike the other services, sentinel is a **client/consumer** of ASCOM devices, not a server.

See [services/sentinel/README.md](services/sentinel/README.md) for usage and [docs/services/sentinel.md](docs/services/sentinel.md) for design documentation.

### Calibrator Flats

Orchestrator plugin for flat field calibration using a CoverCalibrator device (flat panel / light box). Connects to `rp` as an MCP client, iteratively determines the correct exposure time per filter to achieve 50% of the camera's well depth, then captures the requested number of flat frames. Manages the full CoverCalibrator lifecycle (close cover, turn on light, capture, turn off, open cover).

See [docs/services/calibrator-flats.md](docs/services/calibrator-flats.md) for design documentation.

### Polar Align

Orchestrator plugin that measures how far an equatorial mount's RA axis is from the refracted celestial pole and guides the operator through correcting it. Connects to `rp` as an MCP client and slews the mount to three RA positions near the pole, capturing and plate-solving an image at each to compute the axis direction (the N.I.N.A. Three Point Polar Alignment method). It then enters a live adjustment phase: capturing and solving continuously while the operator turns the mount's azimuth/altitude adjusters, publishing the residual error after every solve.

See [docs/services/polar-align.md](docs/services/polar-align.md) for design documentation.

### Sky Survey Camera

ASCOM Alpaca Camera **simulator** that synthesises exposures from NASA SkyView cutouts. Given a configured optical system (focal length, sensor pixel count, pixel size) and a sky position (RA/Dec, settable at runtime via a custom HTTP endpoint), it returns an `ImageArray` matching the field of view the equivalent real telescope would see. Useful for driving ASCOM clients and the rest of the rusty-photon stack end-to-end without hardware.

See [docs/services/sky-survey-camera.md](docs/services/sky-survey-camera.md) for design documentation.

### Star Adventurer GTi

ASCOM Alpaca Telescope driver for the Sky-Watcher Star Adventurer GTi, an entry-level GoTo equatorial mount. Speaks the Sky-Watcher motor-controller protocol over USB-CDC serial (115200 baud) and UDP (192.168.4.1:11880 in mount-AP mode). Implements connect/disconnect, RA/Dec reads, sync, async slew, sidereal tracking, software park, abort, and pulse guiding — leaving custom tracking rates and Alt/Az slew for follow-up. The shared codec lives in the `skywatcher-motor-protocol` workspace crate so other Sky-Watcher mounts can reuse it.

See [docs/services/star-adventurer-gti.md](docs/services/star-adventurer-gti.md) for design documentation.

### Pa Falcon Rotator

ASCOM Alpaca Rotator + Switch driver for the Pegasus Astro Falcon Rotator (firmware ≥ 1.3). Exposes the rotator as `IRotatorV4` with sky/mechanical position separation (`Sync` is a driver-side offset; the wire-level `SD` command is never issued) and a second `ISwitchV3` device that surfaces the Falcon's raw input voltage and `FA.limit_detect` flag as two read-only switches. Communicates via 9600-baud USB-CDC serial; every property read maps to a live serial command (no cache, no background poller) so the device is always the authoritative source.

See [docs/services/falcon-rotator.md](docs/services/falcon-rotator.md) for design documentation.

### DSD FP2

ASCOM Alpaca CoverCalibrator driver for the Deep Sky Dad Flat Panel 2 (FP2), a motorised flat-field panel combining a 4096-step EL light source with a servo-driven cover. Built on the workspace's `rusty-photon-shared-transport` crate (PR #269): the FP2's bracketed-ASCII protocol (`[GFRM]`, `[STRG270]`, `[SLBR1234]`, …) is plugged in as an `Fp2Codec`, `Fp2SerialTransportFactory` opens the USB-CDC port (115200 baud, `/dev/ttyACM*`), and a thin `FlatPanelManager` over `SharedTransport<Fp2Codec>` handles refcounting, request arbitration, and the polling task via `Hooks`. Pairs with `calibrator-flats` for automated flat-field calibration without any orchestrator changes.

See [docs/services/dsd-fp2.md](docs/services/dsd-fp2.md) for design documentation.

### UI-HTMX

Browser-facing, server-rendered configuration UI — a standalone backend-for-frontend (BFF) that holds no UI logic inside `rp`. Renders HTML with axum + Maud and adds interactivity with HTMX. It is a **client** of the drivers, reading and writing each one's configuration through the cross-driver `config.get` / `config.apply` / `config.schema` ASCOM actions, so a single UI can configure any driver without driver-specific knowledge.

See [docs/services/ui-htmx.md](docs/services/ui-htmx.md) for design documentation.

### Plate Solver

An **rp-managed** HTTP service that wraps an operator-supplied [ASTAP](https://www.hnsky.org/astap.htm) CLI install and exposes a narrow solve API to `rp`. Plate solving is a hang-prone, crash-prone external binary, so it runs in its own supervised process where its failure modes cannot threaten `rp`'s liveness. Stateless across requests: every solve spawns a fresh `astap_cli` subprocess under a wall-clock timeout.

See [docs/services/plate-solver.md](docs/services/plate-solver.md) for design documentation.

### QHY Camera

ASCOM Alpaca **Camera (+ FilterWheel)** driver for real QHYCCD hardware, built natively on the vendored first-party `qhyccd-rs` bindings crate (ADR-009). It enumerates every connected QHY camera and CFW and exposes each as an ASCOM device on one port. Implemented (v0). By default the build links the proprietary QHYCCD SDK; for an SDK-free build set `QHYCCD_SKIP_NATIVE_LINK=1` together with `--features simulation` (which `cfg`s out the native FFI) — the path CI and the sanitizer jobs use. See [docs/services/qhy-camera.md](docs/services/qhy-camera.md) for design documentation.

### ZWO Camera

ASCOM Alpaca **Camera** driver for ZWO ASI hardware, built natively on the vendored first-party `zwo-rs` bindings crate (ADR-008 / ADR-010). The MIT ZWO SDK itself is not vendored — it is provisioned at build time; per ADR-014 this binary links only the camera SDK (`libASICamera2`), unless `ZWO_SKIP_NATIVE_LINK=1` is set (the simulation-only path CI uses). Exposes the full `Device + Camera` surface — exposure state machine, ROI/binning, gain/offset, cooling, readout modes, and ST4 pulse guiding — and passes ConformU. Per ADR-014, each independently usable ZWO device gets its own service: the EAF focuser is `zwo-focuser` below, and EFW filter-wheel support will be a separate `zwo-filterwheel` service. See [docs/services/zwo-camera.md](docs/services/zwo-camera.md) for design documentation.

### ZWO Focuser

ASCOM Alpaca **Focuser** driver for the ZWO EAF, built on the same vendored `zwo-rs` crate (its `focuser` feature — the binary links only the focuser SDK, `libEAFFocuser`; ADR-014) rather than the serial transport pattern the other focusers use. Exposes the full `Device + Focuser` surface (absolute move, halt, live temperature) and passes ConformU against the simulation backend; real-hardware validation is pending. See [docs/services/zwo-focuser.md](docs/services/zwo-focuser.md) for design documentation.

### Planetarium Bridge

Virtual ASCOM Alpaca **Telescope** that planetarium apps (SkySafari 7+, Stellarium, Cartes du Ciel) connect to as if it were a mount. Pressing **Align** imports the selected coordinates as a paused target into rp's target store (named by rp's reverse catalog lookup); slews are simulated motion and never import. Imports spool durably on disk while rp is unreachable and replay in order once it is back. The service never touches hardware and is never on the imaging path. See [docs/services/planetarium-bridge.md](docs/services/planetarium-bridge.md) for design documentation.

### Doctor

One-shot CLI that diagnoses a multi-service install and repairs it: packages put bytes on disk, services self-create their configs, and `rusty-photon-doctor` reports what does not line up — unparseable configs, port collisions, dangling cross-service name references, units that will never start, and sentinel's restart-privilege gap — then wires it with `--fix`. It also owns the hardware checks that need no vendor SDK (device nodes, group access, udev rules, USB presence, firmware helper), aggregates each service's own `doctor` subcommand for the SDK-gated ones, and owns the TLS and credential lifecycle (cert issuance, ACME, `tls renew`, credential mint and rotation). Its service catalog is derived from each service's `pkg/doctor.toml`, never hand-maintained. Ships in sentinel's packages alongside the daily renewal timer (ADR-016). See [docs/services/doctor.md](docs/services/doctor.md) for design documentation.

## Getting Started

### Prerequisites

- **Rust** (edition 2021, MSRV 1.94.1 — inherited by all workspace members)
- **[Bazel](https://bazel.build/)** via bazelisk (version pinned by `.bazelversion`) — the local build/test loop and the per-PR CI gate
- **[cargo-nextest](https://nexte.st/)** (`cargo install cargo-nextest --locked`) — optional; used by the nightly Cargo safety net (`act` / raw-cargo fallback)
- **Vendor camera SDKs** (ZWO ASI/EFW/EAF + QHYCCD) — **required**: `bazel build //...` includes the `zwo-camera` / `zwo-focuser` / `qhy-camera` packages, which link the native SDKs (the shared Bazel `zwo-rs` targets build the union of device features, so all three ZWO blobs are needed there; per-service cargo builds link only their own — ADR-014). Install them per [`services/zwo-camera/README.md`](services/zwo-camera/README.md) and [`services/qhy-camera`](services/qhy-camera/) (the same SDKs CI provisions).

### Building

Bazel builds and fast-tests the workspace and is the per-PR CI gate; the Cargo build is the nightly safety net and still drives dependency versions.

```bash
# Build + fast-test everything (the local loop and the per-PR gate)
bazel build //... && bazel test //...

# A single package
bazel build //services/filemonitor/...
```

### Running

```bash
# Run a service (example: filemonitor)
cargo run -p filemonitor -- --help

# Run sentinel with a config file
cargo run -p sentinel -- -c services/sentinel/examples/config.json
```

### Deploying

Every service ships as a `.deb` / `.rpm` package (`rusty-photon-<svc>`),
built natively on the target machine via `scripts/build-packages.sh` —
each daemon with a hardened systemd unit, `phd2-guider` as a plain CLI
tool. See [docs/packaging.md](docs/packaging.md) for building,
installing, configuring, and camera-SDK specifics.

## Testing

The project uses a layered test strategy. See [docs/skills/testing.md](docs/skills/testing.md) for the full testing guide.

```bash
# Run the fast tests (unit; BDD/conformu excluded by default)
bazel test //...

# A single service's tests
bazel test //services/filemonitor/...

# The BDD suites (need OmniSim + OMNISIM_PATH; the binary must be our
# patched fork, release v0.5.0-467.2 or newer — the harness spawns it
# with the fork-only --multi-instance flag + OMNISIM_SETTINGS_DIR)
bazel test --test_tag_filters=bdd //...
```

### ConformU (ASCOM Compliance)

ASCOM Alpaca compliance testing is integrated via [ConformU](https://github.com/ASCOMInitiative/ConformU):

```bash
# Install ConformU (first time only)
./scripts/test-conformance.sh --install-conformu

# Run a service's ConformU suite. The tests are feature-gated behind `conformu`
# (a no-op without it). The canonical per-service command lives in each
# Cargo.toml's [package.metadata.conformu], e.g. for filemonitor:
cargo test -p filemonitor --features conformu --test conformu_integration -- --nocapture
```

### Local CI / pre-push

The local pre-commit gate is Bazel plus Cargo's linters (Bazel runs neither rustfmt nor clippy) — see [docs/skills/pre-push.md](docs/skills/pre-push.md):

```bash
bazel build //... && bazel test //...                     # build + fast tests (BDD/conformu auto-excluded)
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --workspace --lib --bins -- -D warnings      # default-features pass — lints what --all-features cfgs out (#988)
```

The full CI workflows (including the nightly Cargo safety net) can be run locally with [act](https://github.com/nektos/act) — see [docs/skills/pre-push.md](docs/skills/pre-push.md).

## Project Structure

```
rusty-photon/
  Cargo.toml              Workspace root with shared dependencies
  MODULE.bazel            Bazel module (deps resolved from Cargo.toml via crate_universe)
  CLAUDE.md / AGENTS.md   Operating rules for AI agents and human contributors
  crates/
    bdd-infra/                       Shared BDD test infrastructure (ServiceHandle + rp-harness)
    rp-auth/                         HTTP Basic Auth utilities (Argon2id + axum, ADR-003)
    rp-catalog/                      Embedded Messier/NGC/IC catalog with name resolution
    rp-ephemeris/                    Astronomical math (Ephemeris + ERFA wrapper + Site)
    rp-fits/                         FITS reader/writer wrapper (ADR-001)
    rp-plate-solver/                 HTTP client for the plate-solver service
    rusty-photon-tls/                TLS serving for inter-service comms (ADR-002; issuance lives in doctor)
    rusty-photon-config/             Config-path + first-run UniqueID + config.get/apply/schema protocol
    rusty-photon-driver/             Shared ASCOM-driver runtime: DriverError + config-action dispatch (ADR-007)
    rusty-photon-i18n/               Workspace Fluent i18n loader + locale resolver
    rusty-photon-i18n-derive/        Proc-macro deriving LocalizedParser for clap structs
    rusty-photon-service-lifecycle/  Unified lifecycle: runtime + signals + optional Windows SCM
    rusty-photon-shared-transport/   Refcounted multi-client transport scaffolding (serial + UDP)
    skywatcher-motor-protocol/       Sky-Watcher motor-controller wire protocol codec (USB + UDP)
    qhyccd-rs/                       Vendored QHYCCD SDK bindings + nested libqhyccd-sys FFI (ADR-009)
    zwo-rs/                          Vendored ZWO ASI/EFW/EAF SDK bindings + nested libzwo-sys FFI (ADR-008/010/014)
    svbony-rs/                       Vendored SVBony camera SDK bindings (hand-written FFI, no bindgen) + nested libsvbony-sys; consumed by services/svbony-camera
  services/
    rp/                    Main application: equipment gateway, event bus, safety enforcer
    filemonitor/           ASCOM SafetyMonitor (file-based)
    ppba-driver/           ASCOM Switch + ObservingConditions (serial)
    qhy-focuser/           ASCOM Focuser (serial)
    dsd-fp2/               ASCOM CoverCalibrator — Deep Sky Dad FP2 (serial)
    pa-falcon-rotator/     ASCOM Rotator + Switch — Pegasus Falcon (serial)
    pa-scops-oag/          ASCOM Focuser — Pegasus Scops OAG (serial)
    star-adventurer-gti/   ASCOM Telescope — Sky-Watcher GTi (USB + WiFi/UDP)
    sky-survey-camera/     ASCOM Camera simulator backed by NASA SkyView
    qhy-camera/            ASCOM Camera + FilterWheel — QHYCCD hardware (implemented v0; vendored qhyccd-rs bindings)
    zwo-camera/            ASCOM Camera — ZWO ASI hardware (implemented; vendored zwo-rs bindings, MIT SDK)
    zwo-focuser/           ASCOM Focuser — ZWO EAF (implemented; vendored zwo-rs bindings, MIT SDK)
    planetarium-bridge/    ASCOM Telescope (virtual) — planetarium Align gestures become paused rp targets
    phd2-guider/           PHD2 client library (TCP/JSON RPC)
    sentinel/              Monitoring service (HTTP consumer)
    calibrator-flats/      Flat-field tool provider (train_flats / take_flats through rp)
    polar-align/           Plate-solving polar alignment orchestrator
    plate-solver/          rp-managed HTTP service wrapping the ASTAP CLI
    ui-htmx/               Server-rendered web configuration UI (BFF)
    doctor/                Install diagnosis CLI (read-only in D2; ADR-016)
  docs/
    services/              Per-service design documentation
    crates/                Per-crate design documentation
    skills/                How-to playbooks for agents and operators
    references/            Protocol and standards reference
    decisions/             Architecture Decision Records (ADRs)
    plans/                 Active migration and roadmap plans
    workspace.md           Workspace architecture and shared patterns
  scripts/                 CI and ConformU setup scripts
  external/
    phd2/                  PHD2 source (git submodule, reference only)
    homebrew-rusty-photon/ Homebrew tap (git submodule)
```

## Documentation

| Document | Description |
|----------|-------------|
| [docs/workspace.md](docs/workspace.md) | Workspace architecture, shared patterns, dependency policy |
| [docs/skills/development-workflow.md](docs/skills/development-workflow.md) | Skill: design-first, test-first development workflow |
| [docs/skills/testing.md](docs/skills/testing.md) | Skill: writing and organizing tests |
| [docs/skills/pre-push.md](docs/skills/pre-push.md) | Skill: running CI quality gates before pushing |
| [docs/skills/coverage.md](docs/skills/coverage.md) | Skill: checking code coverage in CI and locally |
| [docs/skills/raspberry-pi-runner.md](docs/skills/raspberry-pi-runner.md) | Skill: setting up the Pi 5 ARM64 nightly self-hosted runner |
| [docs/skills/rig-development.md](docs/skills/rig-development.md) | Skill: fast dev loop against the telescope field rig |
| [docs/references/ascom-alpaca.md](docs/references/ascom-alpaca.md) | ASCOM Alpaca protocol reference |
| [docs/decisions/](docs/decisions/) | Architecture Decision Records |

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT) at your option.

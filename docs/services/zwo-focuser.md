# ZWO EAF Focuser Service Design

> **Status: v0 validated on real hardware (2026-07-09 implemented, 2026-07-14
> hardware-validated).** The full `Device` + `Focuser` surface is built over a
> `zwo_rs::Focuser` FFI handle (the `libEAFFocuser` link directive, previously
> deliberately omitted, lands alongside it). Validated end-to-end against a
> physical EAF with temperature probe: enumeration, serial-derived identity,
> connect/move/halt/settle, temperature, range rejection — and **ConformU
> passes with zero errors, warnings or issues against the real device**.
> Hardware validation surfaced two fixes now baked in: `MaxStep` comes from
> `EAFGetMaxStep` (the firmware's working travel limit), not
> `EAF_INFO::MaxStep` (see *Hardware Constraints*), and the EAF is a USB HID
> device whose `/dev/hidraw*` node needs a udev rule that actually reaches
> hidraw (see *Packaging*). 25 unit tests + 27 BDD scenarios (3 feature
> files) are green against the `zwo-rs` simulation backend, whose movement
> model now mirrors the measured hardware behavior (multi-poll IsMoving,
> live position ramp, halt-freezes-mid-travel).
>
> **An EAF that leaves the bus reads disconnected (C5, 2026-10-07).** What
> the EAF SDK answers when an open EAF leaves was measured on the field rig
> first, and the contract follows the camera drivers' departure design. The
> measurements are in
> [`docs/validation/2026-10-07-zwo-focuser-departure-linux/`](../validation/2026-10-07-zwo-focuser-departure-linux/README.md).

## Overview

The `zwo-focuser` service is an ASCOM Alpaca **Focuser** driver for the ZWO EAF
(Electronic Auto Focuser). It talks to the hardware over native ZWO SDK FFI
(`libEAFFocuser`) via the vendored [`zwo-rs`](../../crates/zwo-rs) crate — the
same crate `zwo-camera` uses for its ASI camera / EFW filter-wheel devices, now
extended with an EAF `Focuser` handle.

It is the direct analogue of [`qhy-focuser`](qhy-focuser.md) and
[`pa-scops-oag`](pa-scops-oag.md) at the ASCOM `Focuser` interface level, but its
transport is architecturally different from both: the EAF is a **native-SDK USB
device**, not a USB-CDC/FTDI serial device, so `zwo-focuser` is built on
`zwo-rs`'s FFI seam (mirroring `zwo-camera`), **not** on
`rusty-photon-shared-transport` (the serial pattern `qhy-focuser`/`pa-scops-oag`
use).

**Provenance.** This closes out the scope `docs/plans/zwo-driver.md` sequenced
from the start — **Camera → EFW filter wheel → EAF focuser** — using the EAF SDK
header (`EAF_focuser.h`) already vendored alongside the ASI/EFW headers in
`crates/zwo-rs/libzwo-sys/`. See [ADR-008](../decisions/008-zwo-camera-native-sdk-ffi.md)
(native SDK FFI decision) and [ADR-010](../decisions/010-vendor-zwo-rs.md)
(vendoring `zwo-rs`); no new ADR is needed — both already cover this addition.

**Not cross-platform by default.** Like `zwo-camera`, this service links a
**native vendor SDK** (`libEAFFocuser`, MIT-licensed) at compile time. See
*Native dependency & build gating*.

## Native dependency & build gating

Identical posture to `zwo-camera` (see `docs/services/zwo-camera.md` "Native
dependency & build gating" for the full rationale) — summarized here:

- The chain is `zwo-focuser → zwo-rs → libzwo-sys →` the ZWO SDK
  (`libEAFFocuser`).
- `libzwo-sys`'s `build.rs` links per device feature
  ([ADR-014](../decisions/014-zwo-per-device-services-and-link-features.md));
  this service builds `zwo-rs` with `focuser` only, so its link is exactly
  `EAFFocuser` (+ the C++ runtime and `libudev`, whose `udev_*` symbols the
  EAF blob references without declaring) — machines compiling this package
  need that SDK installed and discoverable. (The shared Bazel `zwo-rs`
  targets build the union of device features, so Bazel actions still
  provision all the blobs.)
- The `simulation` feature makes the build **hardware-free, not SDK-free**: it
  fabricates a fake EAF (position/moving/temperature) at runtime; the native SDK
  is still required at link time. `ZWO_SKIP_NATIVE_LINK=1` omits the link
  directives entirely for sanitizer/simulation-only CI jobs, exactly as it does
  for `zwo-camera`.
- **CI provisioning:** `.github/actions/install-zwo-sdk` already downloaded and
  staged `libEAFFocuser` on all three OSes in anticipation of this service, but
  tolerated a missing/failed blob (`|| true` on Linux/macOS, an optional
  download on Windows) since nothing linked it unconditionally yet. Landing
  this service made that link real, so the action was tightened to fail fast
  instead of deferring to a later, less actionable link/runtime error — and a
  macOS-only gap surfaced by CI was fixed alongside it: the `install_name_tool
  -id` rewrite that makes the SDK's dylibs resolvable to Bazel's `rust_test`
  binaries (no `@rpath` entries) covered `ASICamera2`/`EFWFilter` but not the
  newly-required `EAFFocuser`.
- **Bazel:** `zwo-rs`/`zwo-rs_sim` already exist as a real/sim two-variant build
  (ADR-010); `focuser.rs` becomes part of both targets' existing
  `glob(["src/**/*.rs"])` with no `BUILD.bazel` change in `crates/zwo-rs`.
  `zwo-focuser` gets its own `BUILD.bazel`, structurally identical to
  `zwo-camera`'s (see *Testing*).

## Architecture

```mermaid
graph TD;
    A[ASCOM Client: rp / NINA / SharpCap] -->|Alpaca HTTP :11124| B[ascom-alpaca Server];
    B --> C[ZwoFocuser<br/>impl Device + Focuser];
    C --> BB[Blocking bridge<br/>tokio::task::spawn_blocking];
    BB --> RS[zwo-rs Sdk/Focuser];
    RS -->|FFI| SDK[libzwo-sys → libEAFFocuser];
    SDK -->|libusb-1.0| HW[ZWO EAF over USB];
    C --> CA[config_actions.rs<br/>config.get/apply/schema];
    M[main.rs<br/>ServiceRunner] --> B;
```

**Key components**

- **`main.rs`** — plain `fn main`, parses clap args, inits `tracing`, runs under
  `ServiceRunner::new("zwo-focuser").with_reload().run_with_reload(...)` per
  [`service-lifecycle.md`](../skills/service-lifecycle.md). No hand-rolled signal
  handling; config bootstrap via `rusty_photon_config::resolve_and_init` with an
  **empty identity-pointer list** (identity is hardware-derived), which still
  materializes the default config file on first start.
- **`lib.rs`** — `ServerBuilder` that, on `build()`, opens the SDK and
  **enumerates every connected EAF** via `zwo_rs::Sdk::focusers()`, registering
  each as an ASCOM `Focuser` device (index 0, 1, 2, …) with a serial-derived
  `UniqueID`. Mirrors `zwo-camera`'s open-briefly-to-mint-identity pattern:
  `EAFGetSerialNumber` requires an open device, so enumeration opens each EAF
  briefly to read its serial, then closes it; the per-device connect handshake
  happens later on `set_connected(true)`.
- **`backend.rs`** — the `FocuserHandle` trait (the SDK seam, mirroring
  `zwo-camera`'s `CameraHandle`): `unique_id`, `info`, `session`, `open`, `close`,
  `position`, `is_moving`, `move_to`, `stop`, `max_step`, `temperature`,
  `reverse`/`set_reverse`. `ZwoFocuserHandle` wraps `zwo_rs::Focuser` behind a
  `parking_lot::Mutex` (the SDK handle is `Send` but `!Sync`). It owns the
  departure check (C5): a failed call asks whether the EAF is still there,
  under the lock the call ran under, and an open finds its EAF by serial.
  `MockFocuserHandle` (unit tests only) forces paths the `zwo-rs` simulation
  can't reach.
- **`focuser.rs`** — `ZwoFocuser` (one instance per discovered EAF) implementing
  `Device` + `Focuser` against the `FocuserHandle` seam. Every blocking SDK call
  runs inside `tokio::task::spawn_blocking`.
- **`config.rs`** — typed `Config` (per-serial device overrides + server port).
- **`config_actions.rs`** — `ConfigurableDriver` impl for `config.get`/`apply`/
  `schema`.

**Concurrency.** The EAF SDK is blocking C FFI and is not safe to call
concurrently on a single handle, so every SDK call funnels through
`spawn_blocking` with one logical owner per device — the same discipline
`zwo-camera` uses. Unlike a camera exposure, an EAF move is not a long-running
integration requiring a detached task with a generation counter: `move_to` starts
the move and returns; the caller polls `is_moving`/`position` for completion, so
there is no in-flight-task cancellation/invalidation machinery to build.

## Hardware Constraints

- **Connection**: USB **HID** (ZWO vendor ID `0x03c3`, shared with ASI cameras
  and EFW — but unlike the cameras, which are bulk-USB/libusb devices, the EAF
  and EFW are HID devices the SDK reaches through `/dev/hidraw*`). Consequence:
  a udev rule that matches only the USB device node (`SUBSYSTEM=="usb"`,
  singular) grants no access; with the hidraw node inaccessible, `EAFGetNum`/
  `EAFGetID` still enumerate (USB descriptors) but `EAFOpen` fails with
  `EAF_ERROR_REMOVED` and a pre-open `EAFGetProperty` returns a degraded
  `EAF_INFO` (`MaxStep` 0). See *Packaging* for the rule this package ships.
- **Two `MaxStep` values — the working limit is the real one.**
  `EAF_INFO::MaxStep` is only the fixed *ceiling* (600000 on the validated
  unit) that the limit can be raised to; the firmware actually stops at the
  user-settable `EAFGetMaxStep` working limit (factory default 60000).
  `EAFMove` validates against the *ceiling*: a target past the working limit
  but within the ceiling is **accepted, and the motor silently stops at the
  limit** (observed on real hardware; ConformU flags it as a Move issue if the
  driver advertises the ceiling). The driver therefore reports and validates
  against `EAFGetMaxStep`, read during enumeration's brief open.
- **Movement timing**: the validated EAF travels ≈ 640 steps per second
  (≈ 64-65 steps observed per 100 ms poll); `EAFIsMoving` stays true across
  many polls for a real move (a 1000-step move settles in ≈ 1.7 s) and
  `EAFGetPosition` ramps live toward the target while moving. `EAFStop`
  freezes the position mid-travel.
- **Absolute-position stepper**: `0..=MaxStep` (working limit), with the
  position counter persisted in the focuser across power cycles.
- **Onboard-flash firmware**: like the ASI camera and EFW, the EAF ships firmware
  in onboard flash — no upload step, no cold-plug helper.
- **Temperature sensor**: `EAFGetTemp` reports a live reading (unlike the Scops
  OAG, which has none). With the plug-in temperature probe attached the reading
  is the probe's (validated: ambient-plausible 20.5 °C on the bench).
- **`EAFGetSerialNumber` requires an open device** (`EAF_ERROR_CLOSED` otherwise),
  mirroring the ASI/EFW serial-read pattern; it may return `NOT_SUPPORTED` on
  older firmware, in which case enumeration falls back to a position-based
  identity (see *Device identity*). The validated unit reports a real serial.
- **`EAFIsMoving`** is a dedicated call with two out-parameters (`pbVal`,
  `pbHandControl`) — cleaner than EFW's `-1`-while-moving sentinel on
  `EFWGetPosition`. `pbHandControl` (whether the physical hand-paddle is driving
  the move) has no ASCOM `Focuser` mapping and is discarded in v0 (see *MVP
  scope*).
- **`EAFGetPosition`** has no moving sentinel — it always returns the live
  (ramping) step count, whether or not the focuser is currently moving.
- **The SDK writes its own log, and aborts the process when it cannot.** EAF
  SDK 1.7.7 logs to `/tmp/zwo/log/eaf_sdk/` through spdlog. When that
  directory is not writable, the first SDK call ends the process with an
  uncaught C++ exception (`spdlog::spdlog_ex` … `Permission denied`), which
  Rust cannot catch. The packaged unit runs with `PrivateTmp=yes` and never
  sees another user's `/tmp/zwo`. A hand-run service or the doctor
  subcommand, run as a different user from whoever created `/tmp/zwo` on a
  shared `/tmp`, dies this way (measured on the field rig, 2026-10-07).

## ASCOM Focuser Mapping

| ASCOM Property/Method | Implementation |
|------------------------|----------------|
| Absolute | `true` (always) |
| IsMoving | `EAFIsMoving` (dedicated call; `pbHandControl` discarded) |
| MaxIncrement | `EAFGetMaxStep` (working travel limit; read at enumeration's brief open) |
| MaxStep | `EAFGetMaxStep` (working travel limit; **not** the `EAF_INFO::MaxStep` ceiling — see *Hardware Constraints*) |
| Position | `EAFGetPosition` (no moving sentinel; ramps live while moving) |
| StepSize | `NOT_IMPLEMENTED` (no prior art; matches `qhy-focuser`/`pa-scops-oag`) |
| TempComp | `false` (stubbed — see note below) |
| TempCompAvailable | `false` |
| Temperature | Live `EAFGetTemp` reading |
| Halt | `EAFStop` (freezes mid-travel) |
| Move | Validates `0..=MaxStep` (working limit), `EAFMove` (absolute-only) |
| InterfaceVersion | `4` (trait default) |

**TempComp is a known limitation, not a firmware-confirmed absence.** Neither
`qhy-focuser` nor `pa-scops-oag` implements real temperature-compensation logic —
both stub `TempComp`/`TempCompAvailable` to `false` and `SetTempComp` to
`NOT_IMPLEMENTED` — so there is no prior art in this codebase for wiring an actual
temp-comp mode, and no confirmed ZWO firmware-level spec for one. `zwo-focuser`
follows the same stubbed precedent rather than inventing untested behavior.
Revisit if EAF firmware is confirmed to support it.

**Absolute-only, no relative move.** The ASCOM `Focuser` trait has a single
`move_(position)` method whose semantics depend on `absolute()`; both existing
focuser drivers return `absolute() = true` and treat `move_` as an absolute move
only. `zwo-focuser` follows the same pattern — there is no relative-move prior
art anywhere in this codebase to diverge from.

## Configuration

The service **enumerates every connected EAF** at startup and registers each as
an ASCOM Focuser device (index 0, 1, 2, …) on the one port, mirroring
`zwo-camera`'s multi-device enumeration (rather than `qhy-focuser`/
`pa-scops-oag`'s single-device shape) — `EAFGetNum` already supports N devices,
and the cost over a single-device design is negligible. The hardware is the
source of truth; config carries only optional per-serial display overrides and
the port.

```jsonc
{
  // Optional per-device overrides, keyed by SDK serial. A device with no entry
  // uses SDK-derived defaults (name from model+serial).
  "devices": {
    "EAF-0a1b2c3d4e5f6071": {
      "name": "Main Focuser",
      "description": "ZWO EAF on the Askar 60F"
    }
  },
  "server": {
    "port": 11124,
    "bind_address": "0.0.0.0",
    "tls": null,
    "auth": null
  }
}
```

The `server` block is the shared `AlpacaServerConfig` from
`crates/rusty-photon-server-config` (see ADR-016): `port`, `bind_address`
(default `0.0.0.0`), optional `discovery_port`, and optional `tls`/`auth`.
Absent `tls`/`auth` means plain, unauthenticated HTTP.

Sections:

- **devices** — Optional per-device override map keyed by **SDK serial** (the
  16-hex `EAFGetSerialNumber` value, or `noserial-{index}` when unsupported).
  Any device without an entry uses SDK-derived defaults.
- **server.port** — Listening port (**11124**, next free in the 1112x family;
  11123 is `pa-scops-oag`). Hard read-only (self-lockout: a port change would
  make the BFF lose the devices).

**Deferred** (see *MVP scope*): `EAFSetMaxStep` as a live, config-editable
knob — v0 reads the `EAFGetMaxStep` working limit once, during enumeration's
brief open, and never writes it.

### Config actions

Standard cross-driver protocol ([`config-actions.md`](config-actions.md)),
implemented generically in `rusty_photon_config::actions` + the ASCOM adapter in
[`rusty-photon-driver`](../../crates/rusty-photon-driver). `config_actions.rs`
supplies `ConfigurableDriver for ZwoFocuserDriver`:

- **Secrets redacted/carried forward:** `server.auth.password_hash`.
- **Locked (identity) fields:** none — UniqueIDs are hardware-derived and not
  stored in config, so there is no identity field to lock (same divergence as
  `zwo-camera`; see *Device identity*).
- **Hard read-only fields:** `server.port` (the BFF cannot follow a port
  rebind).
- **Editable fields:** the `devices` map (per-serial `name`/`description`).

`config.apply` persists atomically, returns `status:"applying"` when a field
changed, and fires the in-process reload (`main.rs` runs under
`with_reload().run_with_reload(...)`).

### Device identity (UniqueID)

Derived from the EAF's hardware serial, the same scheme as `zwo-camera`'s ASI
cameras — but with a **simpler fallback chain**, since the EAF SDK exposes no
`ASIGetID`-equivalent second tier:

1. `EAFGetSerialNumber` (open briefly → read → close) — the canonical identity.
2. Otherwise `noserial-{index}` — a stable position-based identity, unique per
   enumeration slot and stable across reconnects for the common single-focuser
   case.

Consequences (same as `zwo-camera`): **no `unique_id` field in config**, an
**empty identity-pointer list** passed to `resolve_and_init` (no minting; the
bootstrap still materializes the default config file on first start), and **no
locked identity field** in the config-actions tiers.

## MVP scope

**In scope (v0)**

- ASCOM Focuser `IFocuserV4` for **every enumerated EAF** (each registered as a
  device on the one port).
- Startup enumeration registers all discovered EAFs; each is opened briefly to
  cache its `EAF_INFO` (name), working travel limit (`EAFGetMaxStep`), and
  serial; per-device connect/disconnect lifecycle re-opens on demand.
- Absolute `Move` validated against `0..=MaxStep`; `Halt`; `Position`;
  `IsMoving`; live `Temperature`.
- `config.get`/`config.apply`/`config.schema` actions; hardware-derived
  `UniqueID`; in-process reload.
- ConformU integration test driven against the `zwo-rs` `simulation` backend.

**Deferred (Future Work)**

- `EAFSetMaxStep` as a live, config-editable knob (v0 reads the working limit
  at enumeration only and never writes it).
- Backlash get/set (`EAFGetBacklash`/`EAFSetBacklash`).
- Beep/buzzer control (`EAFGetBeep`/`EAFSetBeep`).
- Battery info, BLE pairing/scan/connect, LED control (none have an ASCOM
  `Focuser` mapping).
- Hand-paddle-in-use surfacing (`EAFIsMoving`'s `pbHandControl` out-parameter is
  read but discarded — no ASCOM property exists for it).
- Real temperature-compensation logic (`TempComp`/`SetTempComp`/
  `TempCompAvailable` stay stubbed — see the ASCOM mapping note above).
- Reverse-direction exposure (`EAFGetReverse`/`EAFSetReverse` exist in `zwo-rs`'s
  `Focuser` wrapper for parity with `efw.rs`'s `FilterWheel::is_unidirectional`,
  but are not yet surfaced through the ASCOM device or config — no `IFocuserV4`
  property maps to it).

## Behavioral contracts

Named, testable behaviours, each mapping to a BDD scenario in `tests/features/`.
ASCOM error names per [`docs/references/ascom-alpaca.md`](../references/ascom-alpaca.md).
The `simulation` backend presents one fabricated **EAF-Simulated** focuser whose
values and movement model mirror the hardware-validated unit: working travel
limit (`EAFGetMaxStep`) 60000, `EAF_INFO::MaxStep` ceiling 600000, and a
deterministic move that advances **640 steps per `is_moving` poll** (one
second of the real EAF's ≈ 640 steps/s travel, keyed to observation instead
of wall time so tests stay deterministic). Position ramps live toward the target across
polls; the poll that reaches the target still reports moving (as the hardware
does); halting freezes the position mid-travel; a move targeted past the
working limit but within the ceiling is accepted and stops at the limit.

### Enumeration & connection lifecycle

- **C0.** At startup `build()` enumerates all connected EAFs and registers each
  as an ASCOM device with its serial-derived UniqueID (opening each briefly to
  read the serial). Zero discovered EAFs is **not** a hard failure — the service
  starts with no Focuser devices, logged at `warn!`; a later reload
  re-enumerates.
- **C1.** `set_connected(true)` on a device opens *that* EAF, found on the bus
  by its serial (C5). On success `Connected = true`. (The name, working travel
  limit, and serial were cached at enumeration.)
- **C2.** `set_connected(true)` with the device's EAF not on the bus, or any
  SDK open failure, returns `NOT_CONNECTED` and `Connected` stays `false`.
  `EAF_ERROR_REMOVED` from `EAFOpen` usually means the `/dev/hidraw*` node is
  not accessible (see *Hardware Constraints*), not that the EAF has gone.
- **C3.** `set_connected(false)` closes that device and returns `NOT_CONNECTED`
  for subsequent operations.
- **C4.** Connect is per-device and independent: connecting/disconnecting one
  EAF does not affect others enumerated on the same service.
- **C5.** **An EAF that has left the bus reads disconnected.** An EAF that
  loses its cable or its power while connected keeps its open SDK handle, and
  `Connected` is this driver's own record of that handle: a connect sets it, a
  disconnect clears it, and nothing else used to change it. A departed EAF
  therefore answered `Connected == true` for as long as the service ran, its
  `Position`, `IsMoving`, `Temperature`, `Halt` and `Move` failed as
  `INVALID_OPERATION`, and rp's reconnect supervisor, which takes
  `Connected == true` as healthy, never re-established it.

  **What the SDK does when an EAF leaves (measured).** On the field rig
  (Raspberry Pi 5, Raspberry Pi OS aarch64, EAF SDK 1.7.7, firmware 3.8.1),
  with the EAF taken off the bus by disabling its hub port
  ([record](../validation/2026-10-07-zwo-focuser-departure-linux/README.md)):

  - **The SDK says so at once.** From the moment the EAF leaves, every call
    that needs the open session answers `EAF_ERROR_REMOVED`: `EAFGetPosition`,
    `EAFIsMoving`, `EAFGetTemp`, `EAFGetMaxStep`, `EAFGetReverse`,
    `EAFGetBacklash`, `EAFGetSerialNumber`, `EAFGetFirmwareVersion`, `EAFStop`
    and `EAFMove`. Only `EAFGetProperty`, which needs no session, answers from
    memory. This is unlike the ASI cameras, whose SDK hides a departure until
    something rescans ([zwo-camera C6](zwo-camera.md#enumeration--connection-lifecycle)).
  - **A rescan drops it.** `EAFGetNum` (1–10 ms) lists only the EAFs still
    there, and from then on the departed EAF's ID answers `INVALID_ID`.
  - **A returned EAF does not rejoin the old session.** Until a rescan, the old
    ID still answers `REMOVED`, even with the EAF back. A rescan lists it
    afresh: under its old ID if an earlier rescan had dropped it, and the old
    session's calls then answer `EAF_ERROR_CLOSED`; or under a new ID
    otherwise, and the old ID then answers `INVALID_ID`. Either way, closing
    the old ID and opening the listed one works.
  - **A rescan leaves a present EAF alone.** Its ID and its session hold.

  **A failure asks whether the EAF is still there.** When the SDK fails a call
  on the open session, the handle decides, before it answers, whether the EAF
  has left. `REMOVED`, `INVALID_ID` and `CLOSED` on an open session all mean
  its ID no longer names an EAF the session holds, and mark the session lost,
  logged once at `warn`. Any other failure asks: the handle reads the position
  on the same ID once more, and one of those three answers to that read means
  the EAF has gone. Anything else leaves the failure standing as what it is:
  a move refused while another runs (M8) still answers `INVALID_OPERATION`,
  and the session stays live. The second read covers a call already in flight
  when the EAF left that failed some other way. It costs one session read
  (about 2 ms) and no rescan, because the EAF SDK reports a departure without
  one. The question is asked under the focuser lock the failed call ran under,
  which is the lock an open and a close take, so the verdict lands only on the
  session that failed, never on one a reconnect has opened since.

  **Only a failure asks.** Nothing else runs the check: no timer, and no check
  on members that do not reach the SDK. An idle EAF that has left still reads
  `Connected == true`, and `MaxStep` still answers from cache, until a client's
  next call reaches the SDK. A client polling `Position` or `IsMoving`, as rp
  does through a move, finds out on its next poll. The camera drivers follow
  the same rule (zwo-camera C6, decided 2026-10-06).

  **What a lost session answers.** `Connected == false`, and every member that
  takes the connected check answers `NOT_CONNECTED` (M14), `MaxStep` and
  `MaxIncrement` included, though they are served from cache. The request
  whose failure found the departure answers `NOT_CONNECTED` too, rather than
  its call site's `INVALID_OPERATION`. It takes that from its own failure,
  which the handle relabels a departure, not from the mark, so a reconnect
  that clears the mark before the request returns cannot turn it back. The
  connected check reads the handle and its mark together under the focuser
  lock, since a close clears the mark as it lets the EAF go.

  **Lost is not closed.** The driver closes nothing on its own: the session
  ends when a client ends it. `Connected = false` releases a lost session
  through the ordinary disconnect (C3), whatever `EAFClose` says about an ID
  that no longer names the EAF, and succeeds. `Connected = true` releases it
  the same way and then connects afresh, so a client that reconnects, as rp's
  supervisor does, gets either a working focuser or C2's failure, never the
  lost session back. Connection changes run one at a time: `set_connected`
  reads the session and acts on it under one lifecycle lock, so two
  `Connected = true` requests after a departure open one fresh session between
  them. What a `Connected` write does from each session is
  `rusty-photon-driver`'s `connected_transition`, shared with zwo-camera and
  svbony-camera.

  **A connect finds its EAF by identity.** A rescan can list a returned EAF
  under a new ID and renumber the SDK's list, so the enumeration index read at
  startup (C0) can name another EAF, or none. Every open therefore rescans and
  looks for this device's EAF by its serial: it opens the listed EAFs that no
  other device of this service holds, the startup index first, reads each
  one's serial, keeps the one that matches and closes the rest. An EAF without
  a serial (`noserial-{index}`) takes the first free one, its startup index
  first. Opening an EAF and reading its serial moves nothing (tenet 3). The
  search is one step: the service's set of held EAFs stays locked from the
  first look to the reservation, and the SDK's list (`zwo_rs::FocuserList`)
  from the rescan to the open, so another device's rescan cannot renumber the
  list between the choice and the open, and two devices opening at once
  cannot take one EAF. A close keeps its EAF reserved until `EAFClose` has
  run, so another device cannot open the ID afresh in between and then have
  its session closed by that close. An EAF that left and came back therefore
  reconnects with a plain `Connected = true` and no reload; one still gone
  fails the open with C2's error.

  **Not measured.** What the motor does when the EAF leaves mid-move: the
  probe moves nothing on the rig's focuser. The departures were made by
  disabling the hub port, which the kernel logs as a USB disconnect while the
  EAF keeps its power; a cable pull or a power cut was not tried. Windows was
  not measured.

### Movement

- **M1.** `Absolute` is always `true`.
- **M2.** `MaxStep`/`MaxIncrement` report the device's cached working travel
  limit (`EAFGetMaxStep`, read at enumeration's brief open — with a fallback
  to the `EAF_INFO::MaxStep` ceiling if that call fails). They are served from
  cache but answer only while connected (M14), so an EAF that has left the
  bus does not keep describing itself (C5).
- **M3.** `Position` reports the current step count (`EAFGetPosition`, no
  sentinel — always live, ramping toward the target during a move).
- **M4.** `Move` to a position within `[0, MaxStep]` starts the move
  (`EAFMove`).
- **M5.** `Move` to a position outside `[0, MaxStep]` (including negative, and
  including targets beyond the working limit that the firmware would accept
  and silently truncate) returns `INVALID_VALUE`; no SDK call is made.
- **M6.** `IsMoving` is `true` while a move is in progress — across several
  polls, proportionally to the distance — and settles to `false` once the
  move completes.
- **M7.** `Position` reflects the target position once the move completes;
  while the move is in progress it reports live intermediate values.
- **M8.** A second `Move` while already moving is rejected (SDK error mapped to
  `INVALID_OPERATION`).
- **M9.** `Halt` stops an in-progress move (`EAFStop`), freezing the position
  mid-travel (not at the original target).
- **M10.** `Halt` on an idle focuser succeeds as a no-op.
- **M11.** `Temperature` returns the live `EAFGetTemp` reading.
- **M12.** `StepSize` returns `NOT_IMPLEMENTED`.
- **M13.** `TempComp`/`TempCompAvailable` return `false`; `SetTempComp` returns
  `NOT_IMPLEMENTED`.
- **M14.** `Move`/`Position`/`Halt`/`IsMoving`/`Temperature`/`MaxStep`/
  `MaxIncrement` while disconnected, or once the EAF has left the bus (C5),
  return `NOT_CONNECTED`. `Absolute`, `TempComp`, `TempCompAvailable` and
  `StepSize` are fixed by the driver, not read from the device, and answer
  either way.

## Service lifecycle (`main.rs`)

Standard shape per [`service-lifecycle.md`](../skills/service-lifecycle.md),
structurally identical to `zwo-camera`'s (see that design doc's "Service
lifecycle" section) — swap `Camera` for `Focuser` throughout, `zwo-camera` for
`zwo-focuser`, and drop the `simulation_empty` / `--filterwheel` specifics that
don't apply here.

## Testing

Layered per [`testing.md`](../skills/testing.md), following `zwo-camera`'s
structure:

- **Unit** (`src/*.rs` `#[cfg(test)]`) — config parse/newtype validation, the
  `FocuserHandle` mock seam covering paths the `zwo-rs` simulation cannot force,
  move-range validation, identity minting (hardware serial vs. `noserial-{index}`
  fallback).
- **BDD** (`bdd-infra::ServiceHandle`, `tests/features/*.feature`) — enumeration/
  connection lifecycle (C0–C4), an EAF that leaves the bus (C5,
  `focuser_departure.feature`), movement (M1–M14), and config actions, driven
  against the `zwo-rs` `simulation` backend. For C5 the service is started
  with the hidden `--simulation-departure-file <path>`: while that file
  exists the simulated EAF is off the bus. The `zwo-rs` simulation answers as
  the measured SDK does: a session's calls answer `REMOVED` from the
  departure on, `INVALID_ID` once a rescan has run while it was gone, and an
  open finds the EAF only while it is on the bus and the last rescan listed
  it. It does not model a blink no call observed, nor a returned EAF's new ID.
- **Hardware** — C5's SDK behaviour was measured with a `ctypes` probe of
  `libEAFFocuser` on the field rig before the contract was written, and the
  service was then run end to end against the same EAF (see the record linked
  from C5).
- **ConformU** (`tests/conformu_integration.rs`, gated by the `conformu`
  feature) — launches the production binary with `--features conformu` and runs
  `bdd_infra::run_conformu("focuser", …)`. Skipped when `CONFORMU_PATH` is unset.

```bash
# Run all tests
bazel test //services/zwo-focuser/...

# Run BDD tests specifically
bazel test --test_tag_filters=bdd //services/zwo-focuser/...

# Run ConformU compliance tests
cargo test -p zwo-focuser --features conformu --test conformu_integration -- --nocapture

# Run in simulation mode
cargo run -p zwo-focuser --features simulation
```

## Real-hardware validation

**Performed 2026-07-14** against a physical EAF with the plug-in temperature
probe attached, on a Linux dev box (`cargo run -p zwo-focuser`, real build, no
`--features simulation`), driven over the Alpaca HTTP API and by ConformU
4.3.0. Results, in the order the original checklist posed them:

- **Linking and enumeration**: `libEAFFocuser` resolves; the unit enumerates
  and registers as device 0 with UniqueID `ZWO:EAF:<16-hex-serial>` —
  `EAFGetSerialNumber` works on this firmware, so the `noserial-0` fallback
  stayed unexercised.
- **Device access is the real prerequisite**: the EAF is a USB **HID** device;
  with `/dev/hidraw*` inaccessible, startup fails at `EAFOpen` with
  `EAF_ERROR_REMOVED` even though USB enumeration works. See *Hardware
  Constraints* and *Packaging*.
- **MaxStep**: the unit reports an `EAF_INFO::MaxStep` ceiling of **600000**
  but a firmware working limit (`EAFGetMaxStep`) of **60000** — and the motor
  silently stops at the working limit when commanded past it (ConformU caught
  this as a Move issue against the original `EAF_INFO`-based implementation).
  The driver now reports/validates the working limit; ConformU passes with
  zero errors, warnings or issues.
- **Movement timing**: ≈ 640 steps per second (≈ 64-65 steps per 100 ms
  poll); `IsMoving` stays true across many polls (1000 steps ≈ 1.7 s) with
  `Position` ramping live in between.
  The simulation's original settle-after-one-poll / jump-to-target model was
  materially wrong and now mirrors the measured behavior (see *Behavioral
  contracts*).
- **Halt**: `EAFStop` halts a real in-progress move, freezing the position
  mid-travel; a second `Move` while moving is rejected by the firmware
  (`EAF_ERROR_MOVING` → `INVALID_OPERATION`).
- **Temperature**: 20.5 °C on the bench — ambient-plausible (probe reading).
- **Position persistence**: the step counter survives power cycles (stored in
  the focuser, not the host).

**Performed 2026-10-07** on the field rig (Raspberry Pi 5, aarch64) for C5, an
EAF that leaves the bus: a `ctypes` probe of EAF SDK 1.7.7, five departure
phases through the service, and ConformU 4.5.0 `alpacaprotocol` and
`conformance`, both clean. Record:
[`docs/validation/2026-10-07-zwo-focuser-departure-linux/`](../validation/2026-10-07-zwo-focuser-departure-linux/README.md).

## Packaging

Packaged as `rusty-photon-zwo-focuser` (`.deb`/`.rpm`) per
[ADR-012](../decisions/012-service-packaging-architecture.md) /
[ADR-013](../decisions/013-native-sdk-payload-policy.md), mirroring
`zwo-camera`'s packaging: binary at `/usr/bin/rusty-photon-zwo-focuser`, hardened
`rusty-photon-zwo-focuser.service`, and a **uniquely-named** udev rule
(`90-rusty-photon-zwo-focuser.rules`, same VID `03c3` content as `zwo-camera`'s —
a separate file so both packages can install their udev rule on the same host
without a filename collision).

**The udev rule must reach the hidraw node, not just the USB node.** The EAF
(like the EFW, and unlike the ASI cameras) is a USB HID device the SDK opens
via `/dev/hidraw*`; with that node inaccessible the device enumerates but
`EAFOpen` fails with `EAF_ERROR_REMOVED` (observed on real hardware). The
packaged rule's `SUBSYSTEMS=="usb", ATTRS{idVendor}=="03c3"` uses udev's
*parent-walk* match keys, which also match the hidraw child of the ZWO USB
device — **empirically confirmed** via `udevadm test` against a real EAF's
hidraw node, so no explicit `KERNEL=="hidraw*"` line is needed. The singular
`SUBSYSTEM=="usb"` form (as in ZWO's own `asi.rules`, which compensates with
an explicit hidraw line) matches only the USB node itself and leaves the
hidraw node root-only.

**The `rusty-photon` group is guaranteed by both package channels.** udev
drops the *entire rule line at parse time* when its `GROUP=` cannot be
resolved (observed on a dev box where the rule file was hand-copied without
any package install), so the group's existence is load-bearing. The rule
assigns the service account's own group — created together with the user
by every package's install scriptlet *before* the udev reload, on both
deb and rpm hosts, so the rule always parses with the group present.
(Debian's `plugdev` is deliberately not used: it does not exist on
rpm-family hosts and is never created there.) A hand-installed rule file
on a host without the rusty-photon account (e.g. a dev box, outside any
package) silently does nothing — use a resolvable group or ZWO's
world-writable rules there.

The MIT-licensed `libEAFFocuser.so` — **exactly the one SDK this binary links**
(zwo-rs `focuser` feature,
[ADR-014](../decisions/014-zwo-per-device-services-and-link-features.md)) —
ships **inside the package**, staged at build time by
`scripts/build-packages.sh` from the same pinned indi-3rdparty ref
`.github/actions/install-zwo-sdk` uses, alongside the SDK license. Because
each zwo package owns only its own blob, this package co-installs cleanly
with `rusty-photon-zwo-camera` (which ships `libASICamera2.so`). ZWO EAFs
keep their firmware in onboard flash, so there is no firmware-install helper.

## Future Work

- `EAFSetMaxStep`/`EAFGetMaxStep` as a config-editable knob.
- Backlash, beep, battery, BLE, LED, and hand-paddle surfacing (see *MVP scope*).
- Real temperature-compensation logic, if a confirmed EAF firmware spec exists.
- Reverse-direction exposure through the ASCOM device/config, if ASCOM tooling
  or a future interface version adds a mapping for it.

## References

- Decision record: [`docs/plans/zwo-driver.md`](../plans/zwo-driver.md) ·
  [ADR-008](../decisions/008-zwo-camera-native-sdk-ffi.md) ·
  [ADR-010](../decisions/010-vendor-zwo-rs.md)
- Same-vendor-class precedent: [`zwo-camera.md`](zwo-camera.md) (native-SDK
  architecture, build gating, Bazel real/sim pattern)
- Same-interface precedent: [`qhy-focuser.md`](qhy-focuser.md) ·
  [`pa-scops-oag.md`](pa-scops-oag.md) (ASCOM Focuser mapping, absolute-only
  move, TempComp stub)
- [`config-actions.md`](config-actions.md) ·
  [`service-lifecycle.md`](../skills/service-lifecycle.md) ·
  [`development-workflow.md`](../skills/development-workflow.md) ·
  [`testing.md`](../skills/testing.md)
- ASI/EFW/EAF SDK (headers + per-arch binaries, MIT): INDI `indi-3rdparty/libasi`
  (`EAF_focuser.h`)

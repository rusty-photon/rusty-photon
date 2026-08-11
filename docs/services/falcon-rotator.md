# pa-falcon-rotator Driver

ASCOM Alpaca Rotator driver for the Pegasus Astro Falcon Rotator.

## Overview

The `pa-falcon-rotator` service exposes the Pegasus Astro Falcon Rotator (firmware ≥ v1.3) as **two** ASCOM Alpaca devices on a single server:

1. An ASCOM **Rotator** device implementing the standard `IRotatorV4` surface: `Position`, `MechanicalPosition`, `TargetPosition`, `IsMoving`, `Reverse`, `Move` / `MoveAbsolute` / `MoveMechanical` / `Sync` / `Halt`.
2. An ASCOM **Switch** device with two read-only switches: `0` = input-voltage reading from `VS` (raw ADC count — scale factor deferred), `1` = limit-hit indicator from `FA.limit_detect`. See [Status Switch Device](#status-switch-device).

The service borrows the **layering** from `ppba-driver` / `qhy-focuser` but deliberately **omits the cached / background-polled state machine** those services carry. Every ASCOM property read maps to a serial command. See [Why no cache](#why-no-cache) for the trade-offs.

- **Codec** (`codec.rs`) — `FalconCodec` implements `rusty_photon_shared_transport::Codec`: `encode` appends `\n`, `decode` dispatches by reply prefix into the `FalconResponse` enum, `matches` enforces variant-shape pairing.
- **Transport factory** (`serial.rs`, `mock.rs`) — `FalconTransportFactory` builds a `SerialFrameTransport` over `tokio-serial` with `\n` framing; `MockFalconTransportFactory` (feature-gated under `mock`) hands out an in-memory state machine for tests.
- **Protocol layer** (`protocol.rs`) — command serialisation, response parsing, and the `validate_echo` helper (used by the manager to confirm echo-bearing replies match the issued command).
- **Manager** (`manager.rs`) — `FalconManager` wraps an `Arc<SharedTransport<FalconCodec>>` plus the three small pieces of driver-side state pinned by the [Sync semantics](#sync-semantics--why-driver-side-not-sd) and [`limit_detect` handling](#limit_detect-handling) sections (`sync_offset`, `target_position`, `last_limit_detected`). Constructs `Hooks { handshake, teardown, while_open: None }` — there is no background poll loop, so the while-open slot is empty. Exposes the protocol API (`read_status`, `read_voltage_raw`, `move_mechanical`, `halt`, `set_reverse`, `sync`) that the device types call through a `&Session<FalconCodec>`.
- **ASCOM devices** (`rotator_device.rs`, `switch_device.rs`) — `Device` + `Rotator` / `Device` + `Switch` trait implementations. Each device holds its own `Option<Session<FalconCodec>>`; `set_connected(true)` calls `transport().acquire()` to obtain one and `set_connected(false)` calls `session.close().await` to release it. The two devices share one `Arc<FalconManager>` and therefore one underlying transport — refcounting on `SharedTransport` is what makes both devices' `Connected=true` calls cooperate on a single open serial port.
- **Server builder** (`lib.rs`) — binds the ASCOM Alpaca server and registers both devices.

## Architecture

```
+-------------------+        FH/FA/MD/SD/...        +----------------------+
|                   |  ASCII over 9600-8N1 serial   |                      |
| pa-falcon-rotator |  ───────────────────────────► |  Falcon Rotator      |
|  (Alpaca server)  |  ◄─────────────────────────── |  (firmware ≥ 1.3)    |
|                   |   FR_OK / FD:.. / FR:.. etc.  |                      |
+--------▲----------+                               +----------------------+
         │ HTTP (port 11118)
         │ /api/v1/rotator/0/...
+--------┴----------+
|  Alpaca client    |
|  (NINA, SGPro,…)  |
+-------------------+
```

A single serial connection is shared by both registered devices and all of their clients. The `FalconManager` wraps a `SharedTransport<FalconCodec>` from `rusty-photon-shared-transport`; refcounting on that transport opens the port on the first `acquire()` (driven by the first `set_connected(true)`) and closes it when the last `Session` is dropped or closed. The transport's command-arbitration lock serialises every device-bound encode → `send_frame` → `recv_frame` → decode pair so concurrent property reads queue cleanly on the one physical port. The `FalconManager` also holds three small pieces of driver-side state: `sync_offset` (sky-vs-mechanical correction), `target_position` (the last requested target in **sky** coordinates), and `last_limit_detected` (used for `limit_detect` edge logging — see [`limit_detect` handling](#limit_detect-handling)). The transport's `Hooks::handshake` runs the documented `F# → FV → DR:0 → FA → VS` sequence atomically — if any step fails the refcount rolls back and the underlying serial port is dropped, so a half-connected state can't escape.

### Why no cache

Every other serial service in this repo (`ppba-driver`, `qhy-focuser`, `star-adventurer-gti`) caches device state behind a `RwLock<CachedState>` updated by a background poller. `pa-falcon-rotator` does not, by design.

The benefit a cache would add — sub-millisecond property reads — matters when many concurrent ASCOM clients hit the same server. We don't expect that here: observatory rotators are typically driven by exactly one session at a time. The costs are real: a polling task with its own lifecycle, a state-completion detector, an "active refresh" patch for `is_moving()` to defeat staleness during moves, and a second source of truth that drifts when the device is mutated out of band by the Pegasus Unity app.

A no-cache design pays one ~10–30 ms serial roundtrip per property read in exchange for always-fresh reads, drift-free state, and `Hooks::while_open = None` on the shared transport (versus the poll loop the other three services configure there). The trade-off is reversible: if production load shows contention, we can layer a TTL cache on top later — and the device-trait implementations need not change, because every property reads through the same `FalconManager` API.

## Hardware Constraints

| Property | Value | Source |
|---|---|---|
| Transport | USB-CDC virtual serial port | vendor |
| Baud rate | 9600 | PDF |
| Framing | 8N1 | PDF |
| Line terminator | LF (`\n`) on both directions | PDF (literal "/n" read as typo) |
| Command-set firmware | Falcon v1, firmware ≥ v1.3 (Sep 2020 review) | PDF |
| Angular unit (driver-facing) | degrees, two decimal places (`MD:nn.nn`) | PDF |
| Angular unit (internal device) | steps (integer); `FA` reports both | PDF |
| Steps per degree | 86.6 | <https://pegasusastro.com/products/falcon-rotator/> |
| Steps per revolution | 31,192 | <https://pegasusastro.com/products/falcon-rotator/> |
| Angular resolution | ~0.01155° per motor step | derived from `1.0 / 86.6` |
| Soft rotation limit | Clockwise rotation refused beyond 220°; the device automatically takes the anti-clockwise path | <https://pegasusastro.com/products/falcon-rotator/> |

Note: the vendor's "86.6 steps/°" and "31,192 steps/revolution" are mutually inconsistent by ~16 steps (`86.6 × 360 = 31,176`). The 0.011541° value (`360 / 31192`) is closer to the truth; 0.01155° is a clean published figure and is what the driver reports. The difference is irrelevant at ASCOM client UI precision.

The Falcon is a **finite-rotation device with mechanical end stops** and an additional firmware-enforced soft limit at 220° CW. Drivers issuing `MD:<deg>` with any value in `[0, 360)` are safe: the Falcon picks the path itself. The `FA.limit_detect` flag fires only on a physical limit hit (mechanical jam or end-stop) — not as part of normal direction selection.

## Protocol Reference

All commands are ASCII, newline-terminated. The PDF's footer literally reads "/n", which we treat as a typo for `\n` (LF). All responses are line-terminated by the device.

### Command Table

| Command | Description | Response |
|---|---|---|
| `F#` | Ping / status check | `FR_OK` |
| `FA` | Full status (see below) | `FR_OK:steps:deg:moving:limit:derot:reverse` |
| `FV` | Firmware version | `FV:n.n` |
| `FD` | Current position in degrees | `FD:nn.nn` |
| `FP` | Current position in steps | `FP:n..` |
| `VS` | Input voltage (raw / unscaled) | `VS:n..` |
| `DR:<ms>` | Enable de-rotation at `<ms>` ms/step; `DR:0` disables | `DR:<ms>` |
| `SD:<deg>` | **Device-side** sync: rewrite the Falcon's stored position counter to `<deg>` *without moving* | `SD:<deg>` |
| `MD:<deg>` | Move to absolute degrees | `MD:<deg>` |
| `MS:<steps>` | Move to absolute steps | `MS:<steps>` |
| `FH` | Halt | `FH:1` |
| `FR` | Is running (`1` = moving, `0` = idle) | `FR:1` or `FR:0` |
| `FN:<b>` | Set motor reverse (1 reverse / 0 normal). **Persisted in EEPROM.** | `FN:1` or `FN:0` |
| `FF` | Reload firmware | *(no response)* |

### `FA` response format

```
FR_OK:position_in_steps:position_in_deg:is_moving:limit_detect:do_derotation:motor_reverse
```

Example: `FR_OK:4332:50.00:0:0:0:0` → 4332 steps, 50.00°, idle, no limit, derotation off, reverse off.

Field types:

| Field | Type | Notes |
|---|---|---|
| `position_in_steps` | **signed** integer (`i32`) | Step counter relative to the 0° home; **negative for positions CCW of home**. Informational only — the driver derives all positions from `position_in_deg`. |
| `position_in_deg` | `f64`, two decimals | Authoritative for `MechanicalPosition`; always wrapped to `[0, 360)` |
| `is_moving` | `0` / `1` | Drives `IsMoving` |
| `limit_detect` | `0` / `1` | Driver logs `warn!` on edge transitions, does not raise |
| `do_derotation` | `0` / `1` | MVP keeps this `0` (see [De-rotation](#de-rotation)) |
| `motor_reverse` | `0` / `1` | Drives `Reverse` |

> **Signed step counter (validated on real hardware, firmware 1.5, ConformU).**
> A target beyond the 220° CW limit is reached the long way round — CCW past the
> 0° home — where `position_in_steps` goes **negative** (e.g. `FR_OK:-2838:327.24:1:0:0:0`)
> while `position_in_deg` stays wrapped to `[0, 360)`. Parsing the step field as
> unsigned previously aborted every status read in that region with
> `INVALID_OPERATION: FA position_steps: invalid digit found in string`; it is
> now parsed as `i32`. The degree field is unaffected, so positions stay correct.

### Commands the driver does **not** send

- **`FF`** — firmware reload is a maintenance operation with no response and unknown post-conditions. The driver never issues it, and excludes it from `SupportedActions` so ASCOM clients cannot trigger it via the `Action` endpoint either. Operators perform firmware updates via the vendor tool.

## ASCOM Rotator Mapping

The Falcon has a single physical-angle concept. ASCOM's `IRotatorV3+` separates *mechanical* angle from *sky* angle joined by a `Sync` offset. The driver implements that separation in software.

The two frames and their bridge are distinct types in `units.rs` — `MechanicalDegrees`, `SkyDegrees`, and the `SyncOffset` that joins them — so the offset arithmetic in the table below is checked at compile time rather than by convention. The whole algebra is three operators plus one rotation:

| Relation | Typed form |
|---|---|
| `Position` = mech + offset | `MechanicalDegrees + SyncOffset → SkyDegrees` |
| `MoveAbsolute`: mech = sky − offset | `SkyDegrees − SyncOffset → MechanicalDegrees` |
| `Sync`: offset = sky − mech | `SkyDegrees − MechanicalDegrees → SyncOffset` |
| `Move(delta)`: mech.rotate(delta) | relative rotation, stays mechanical |

A sky angle therefore can't be used where a mechanical one is expected, and the offset can't be silently dropped, without a compile error. All three angle types normalise to `[0, 360)` on construction (see [Position normalisation](#position-normalisation)); non-finite inputs are rejected at the ASCOM boundary before a value is constructed. `Steps` (the signed `FA`/`FP` counter) is a fourth type, but since the driver derives every position from the degree field it is informational on the wire — only the mock converts between `Steps` and `MechanicalDegrees` (× 86.6).

| ASCOM Property/Method | Implementation |
|---|---|
| `CanReverse` | `true` (always) |
| `IsMoving` | Issues `FA`, returns `FA.is_moving == 1`. Always fresh from the device. |
| `Position` | Issues `FA`, returns `(FA.position_in_deg + sync_offset) mod 360`. |
| `MechanicalPosition` | Issues `FA`, returns `FA.position_in_deg` normalised to `[0, 360)`. |
| `TargetPosition` | If a `Move*` is outstanding, returns the sky-coordinate target stored in `FalconManager::target_position: Mutex<Option<f64>>`. Otherwise (no move ever requested, or last move was halted, or the device is idle after a completed move) returns the current `Position`. Treating "where I am" as the implicit target when nothing else is in flight keeps ConformU happy and matches the dominant convention among ASCOM rotator drivers. |
| `Reverse` (get) | Issues `FA`, returns `FA.motor_reverse`. |
| `Reverse` (set) | EEPROM-wear-aware: issues `FA` first, only writes `FN:b` if the requested value differs from the device's current value. See [Reverse semantics](#reverse-semantics--eeprom-persistence). |
| `StepSize` | `0.01155` (constant). Per the vendor product page, the Falcon does 86.6 steps per degree → `1.0 / 86.6 ≈ 0.01155 °/step`. Reported as `f64` even though the `MD:nn.nn` wire format limits commandable values to 0.01° precision; ASCOM clients that round move targets to `StepSize` will land within one motor step of the requested angle. |
| `Halt()` | `FH`. Clears `TargetPosition` and `is_moving` on success. |
| `Move(delta)` | Relative move: `target = (MechanicalPosition + delta - sync_offset) mod 360`, then `MD:<target>`. ASCOM defines `Move` in **sky** coordinates, so the same `sync_offset` is applied. |
| `MoveAbsolute(skyDeg)` | `target_mechanical = (skyDeg - sync_offset) mod 360`, then `MD:<target_mechanical>`. |
| `MoveMechanical(mechDeg)` | Wire command: `MD:<mechDeg mod 360>` directly — `sync_offset` is **not** applied to the wire value. Internal `target_position` is still stored in sky coordinates (`mechDeg + sync_offset`) so subsequent `TargetPosition` reads stay in the same frame regardless of which `Move*` variant set them. |
| `Sync(skyDeg)` | `sync_offset = (skyDeg - MechanicalPosition) mod 360`. **Driver-side only.** The Falcon's `SD` command is *not* used here: ASCOM Sync must leave `MechanicalPosition` unchanged, but `SD` rewrites the device's stored counter. |

All command issuance flows through the device's held `Session<FalconCodec>` — devices borrow it via an internal `with_session` helper, which short-circuits to `NotConnected` if the session slot is empty.

### Sync semantics — why driver-side, not `SD`

ASCOM's `Sync(skyAngle)` is specified as: *the value reported by `Position` becomes `skyAngle`, the value reported by `MechanicalPosition` is unchanged*. The Falcon's `SD:<deg>` command rewrites the device's stored counter, which would change `MechanicalPosition` on the next `FA` read and break the ASCOM contract. The driver therefore tracks the offset in software as `FalconManager::sync_offset: Mutex<f64>` (initialised to `0.0` on connect, reset by `FalconManager::clear_session_state` when the last device disconnects) and never issues `SD`.

A side-effect of this choice: the sync offset is **not** persisted across driver restarts. If a deployment needs persistent sync, that is a follow-up — see [Open questions](#open-questions).

### Move target storage

`FalconManager` holds `target_position: Mutex<Option<f64>>` storing the **sky-coordinate** target (so `TargetPosition` reads don't need to re-apply the offset). Lifecycle:

1. `Move(delta)` / `MoveAbsolute(skyDeg)` / `MoveMechanical(mechDeg)` each compute the sky target, write `target_position = Some(sky_target)`, convert to mechanical, then issue `MD:<mech>`.
2. While the move is in progress, `is_moving()` reads `FA.is_moving` directly from the device on every call. There is no driver-side "is the move done?" flag — the device is the authoritative source.
3. `TargetPosition` reads return the stored sky target if `Some`; otherwise they return the current `Position` (one fresh `FA` read).
4. `target_position` is cleared (`None`) in exactly two places: on `Halt`, and on the *next* `Move*` (the new target overwrites; this is a momentary transition, not a clear-then-set). A **successful move does not clear `target_position`** — the stored target remains readable so clients can verify "did I land where I asked?" by comparing `Position` to `TargetPosition` after `IsMoving` falls false.

The Falcon's `is_moving` flag is the authoritative completion signal. We deliberately do **not** compare `FA.position_in_deg` to `target_position` for completion — angular comparison would have to handle backlash, overshoot, and wraparound, and the device already tells us the answer.

> **Note on storage frame.** Storing the **sky** target (not mechanical) means `TargetPosition` reads are stable across mid-flight syncs (rare in practice, but possible if a client misbehaves). The driver applies the `sync_offset` once at write time to compute the mechanical `MD:` argument. Reads don't re-apply the offset, so the value the client gets back is the same value it asked for.

### Reverse semantics — EEPROM persistence

`FN:b` is documented as "One off setting — stored in EEPROM". The driver therefore protects the EEPROM by reading before writing:

1. `Reverse` get → single `FA`, returns `motor_reverse`. Operator changes via the Pegasus Unity app are visible on the very next get.
2. `Reverse` set → `FA` first; if the device already reports the requested value, the driver returns success without writing. Only if the values differ does the driver issue `FN:b` and validate the echo.

The extra `FA` per set is a one-roundtrip cost (~30 ms), paid only when an ASCOM client calls `setreverse`, which is rare.

### Position normalisation

All angles exposed to ASCOM clients are normalised to `[0.0, 360.0)`. The normalisation rule is the standard `((x % 360.0) + 360.0) % 360.0` to handle negative deltas from `Move(delta < 0)`.

### `limit_detect` handling

`FA.limit_detect == 1` indicates the rotator hit a mechanical end stop on the most recent move. The driver surfaces this two ways:

1. **`warn!` log on the rising edge.** `FalconManager` holds `last_limit_detected: Mutex<Option<bool>>`. Every `FA` response parsed via `FalconManager::read_status` runs the edge check — the rule itself lives in the pure `is_limit_detect_rising_edge(previous, current)` helper — and on a `false → true` (or `None → true`) transition the driver logs `warn!("Falcon reported limit_detect after move toward {:?}", target_position)`. The state is initialised to `None` by the connect-time handshake hook, so a fresh connection whose very first observation reports `limit_detect = 1` still logs — the `None → true` arm exists precisely so that case is not swallowed while waiting for a `false → true` transition that may never come. A fresh connection reporting `limit_detect = 0` logs nothing.
2. **Read-only Switch ID `1`.** The Status Switch device exposes `limit_detect` as a boolean switch (`MinSwitchValue = 0`, `MaxSwitchValue = 1`, `SwitchStep = 1`). `GetSwitch(1)` and `GetSwitchValue(1)` each issue a fresh `FA` and return the value verbatim. Operator dashboards (sentinel, custom monitors) can poll this switch to alert on limit hits.

The driver does **not**:

- Raise an ASCOM error from `Move*` when a limit was hit (the call returned `Ok` long before the device stopped).
- Pre-validate move targets against any soft range — the Falcon owns its own travel limits.
- Latch `limit_detect` itself — we mirror whatever the device currently reports. If the Falcon clears the flag on the next successful move, the Switch clears too.

## De-rotation

The Falcon's `DR:<ms>` enables a free-running rotation intended for alt-az field derotation. ASCOM's `IRotator` has no derotation concept and assumes discrete moves to a target.

**MVP behaviour:** the driver issues `DR:0` once during the connect handshake to guarantee a known idle state. The driver does **not** expose any way to *enable* derotation. The `FA` field is parsed and logged but not surfaced over Alpaca.

Adding derotation later is tracked in [Open questions](#open-questions).

## Status Switch Device

The Falcon reports two non-rotator state values that fall outside the ASCOM Rotator surface: an input-voltage reading (`VS`, "raw format" per the PDF) and a limit-detect flag (`FA.limit_detect`). Both are exposed as read-only switches on a second ASCOM device registered on the same Alpaca server.

ASCOM `IObservingConditions` was considered for voltage and rejected: its standard properties are strictly weather data (`Temperature`, `Humidity`, `Pressure`, `DewPoint`, `CloudCover`, etc.) and voltage is not in that set. ConformU would flag an `ObservingConditions` device that exposed only voltage. `ppba-driver` reaches the same conclusion — its `Switch` device carries voltage (switch `10`), and its `ObservingConditions` device carries only the temp/humidity/dewpoint trio.

### Switch layout

| ID | Name | Type | Min | Max | Step | Source | Notes |
|---|---|---|---|---|---|---|---|
| 0 | Input Voltage (raw) | Numeric | 0 | 1023 | 1 | `VS` | Raw ADC count. Scale factor is **not** applied — see [Scale calibration](#scale-calibration). |
| 1 | Limit Hit | Boolean | 0 | 1 | 1 | `FA.limit_detect` | Mirrors the device's current limit-detect flag. See [`limit_detect` handling](#limit_detect-handling). |

`MaxSwitch = 2`. `CanWrite(0) = false`, `CanWrite(1) = false`.

- `GetSwitch(0)` returns `true` if `voltage_raw > 0` (per ASCOM's "false at MinSwitchValue, true otherwise" rule).
- `GetSwitch(1)` returns `true` iff a limit is currently detected.
- `GetSwitchValue(0)` issues `VS` and returns the parsed raw ADC count as `f64`.
- `GetSwitchValue(1)` issues `FA` and returns `0.0` or `1.0`.

`Min = 0`, `Max = 1023` for switch 0 assumes a 10-bit ADC on the Falcon's MCU. If hardware characterisation reveals a wider range (12-bit / 16-bit) the `Max` value is widened in a follow-up; the field is hard-coded rather than configurable to keep the ASCOM metadata stable across deployments.

#### Write surface (read-only device)

Because both switches are read-only, the write surface is a capability gap rather than a state-dependent rejection:

- `SetSwitch`, `SetSwitchValue`, `SetSwitchName` return `NOT_IMPLEMENTED` (`0x400` / 1024) for any valid id. ConformU treats `INVALID_OPERATION` (`0x40B` / 1035) as the wrong code here — that code is for "the device is in a state where this operation isn't allowed", not for "this device fundamentally doesn't support this operation".
- The ISwitchV3 async surface (`CanAsync`, `SetAsync`, `SetAsyncValue`, `CancelAsync`) is explicitly overridden so id validation runs before the trait defaults. `CanAsync(valid_id)` returns `false`; the three writers return `NOT_IMPLEMENTED`. Out-of-range ids return `INVALID_VALUE` (1025) on every async method.
- Every write method still runs the `ensure_connected!` guard first, so a disconnected client always sees `NOT_CONNECTED` (1031) regardless of id or method.

### Scale calibration

The voltage raw-to-volts conversion is **deliberately deferred**. The PDF gives no scale factor, and we have no schematic for the input divider. Two follow-up paths once hardware characterisation is in hand:

1. Replace the raw-ADC switch with a calibrated `Input Voltage (V)` switch using a hard-coded scale derived from the bench measurement. Simplest if the conversion is firmware-stable.
2. Add `voltage_scale: f64` and `voltage_offset: f64` to the rotator config and report `volts = raw * scale + offset`. Lets the operator re-calibrate without a code change.

Either path is a follow-up PR. The MVP just exposes the raw count.

### Reads

`VS` is issued on demand inside `GetSwitchValue(0)`. `FA` is issued on demand inside `GetSwitch(1)` / `GetSwitchValue(1)` and shares its parse path with `Rotator.IsMoving` — both route through `FalconManager::read_status()`, and the `last_limit_detected` edge tracker fires from there regardless of which device initiated the read.

## Configuration

```json
{
  "serial": {
    "port": "/dev/ttyUSB0",
    "baud_rate": 9600,
    "timeout": "2s"
  },
  "server": {
    "port": 11118,
    "bind_address": "0.0.0.0",
    "auth": {
      "username": "observatory",
      "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$..."
    }
  },
  "rotator": {
    "name": "Pegasus Falcon Rotator",
    "unique_id": "",
    "description": "Pegasus Astro Falcon Rotator (firmware >= 1.3)",
    "enabled": true
  },
  "switch": {
    "name": "Pegasus Falcon Status",
    "unique_id": "",
    "description": "Pegasus Falcon Rotator status sensors (voltage + limit-hit)",
    "enabled": true
  }
}
```

| Section | Field | Description | Default |
|---|---|---|---|
| `serial` | `port` | Serial port path | `"/dev/ttyUSB0"` on Unix, `"COM3"` on Windows (placeholder — edit to the real port) |
| `serial` | `baud_rate` | Baud rate | `9600` |
| `serial` | `timeout` | Serial read/write timeout (`humantime`) | `"2s"` |
| `server` | `port` | HTTP server port | `11118` |
| `server` | `bind_address` | Interface to bind (`0.0.0.0` listens on all interfaces) | `"0.0.0.0"` |
| `server` | `discovery_port` | Alpaca UDP discovery responder port (opt-in; normally `32227`). Absent/`null` disables discovery — many rusty-photon servers on one host would collide on the shared port | _absent_ (disabled) |
| `server` | `tls` | Optional `rusty-photon-tls` block | none |
| `server` | `auth` | Optional `rp-auth` block | none |
| `rotator` | `name` | ASCOM device name | `"Pegasus Falcon Rotator"` |
| `rotator` | `unique_id` | Stable ASCOM UniqueID (see [Device identity](#device-identity-uniqueid)) | _minted UUIDv4 on first run_ |
| `rotator` | `description` | Device description | `"Pegasus Astro Falcon Rotator (firmware >= 1.3)"` |
| `rotator` | `enabled` | Whether to register the Rotator device | `true` |
| `switch` | `name` | ASCOM device name for the Switch | `"Pegasus Falcon Status"` |
| `switch` | `unique_id` | Stable ASCOM UniqueID for the Switch (see [Device identity](#device-identity-uniqueid)) | _minted UUIDv4 on first run_ |
| `switch` | `description` | Switch description | `"Pegasus Falcon Rotator status sensors (voltage + limit-hit)"` |
| `switch` | `enabled` | Whether to register the Switch device | `true` |

The `server` block is the shared `AlpacaServerConfig` from
`crates/rusty-photon-server-config` (see ADR-016): `port`, `bind_address`
(default `0.0.0.0`), optional `discovery_port`, and optional `tls`/`auth`.
Absent `tls`/`auth` means plain, unauthenticated HTTP.

Every block (`Config` and each nested config struct) rejects unknown keys at deserialize (`deny_unknown_fields`), so a typo or a key removed by a schema change fails loudly at load instead of being silently ignored.

### Device identity (UniqueID)

Each device's `unique_id` is its stable ASCOM **UniqueID**. The driver mints a
fresh **UUIDv4** for the rotator and for the status switch on first run (via the
shared `rusty-photon-config` crate), persists them to the resolved config file,
and **never overwrites** an existing id — so identities are globally unique and
stable across restarts, as the ASCOM spec requires. First run now **creates the
config file** (at `--config`, else the platform default —
`~/.config/rusty-photon/pa-falcon-rotator.json` on Linux,
`%PROGRAMDATA%\rusty-photon\pa-falcon-rotator.json` on Windows) if it is
absent. Leave `unique_id` empty (or omit it) to have one
minted; set it explicitly only to migrate a known id.

### CLI Arguments

| Argument | Description |
|---|---|
| `-c, --config` | Path to configuration file |
| `--port` | Serial port path (overrides config) |
| `--server-port` | Server port (overrides config) |
| `-l, --log-level` | Log level: `trace`, `debug`, `info`, `warn`, `error` |
| `--service` | Hidden: run as a Windows service (passed by the Windows service control manager; no-op on other platforms) |

`pa-falcon-rotator doctor [--config <file>] [--json]` diagnoses this service's
own config read-only without starting it — see
[doctor.md §Per-service doctors](doctor.md). Top-level flags cannot be
combined with the subcommand (the mixed form would silently ignore them).

### Config actions

Both devices expose the configuration over HTTP as the vendor ASCOM actions
`config.get` / `config.apply` / `config.schema` — the cross-driver protocol in
[`config-actions.md`](config-actions.md), implemented generically in
`rusty_photon_config::actions`. `config_actions.rs` supplies the driver-specific
half (`ConfigurableDriver for FalconRotatorDriver`) **and** the shared `dispatch`
both `rotator_device.rs` and `switch_device.rs` delegate to, so an apply on
either device operates on the one full driver config and fires the one reload
(`ReloadSignal::notify` coalesces).

- **Secret redacted / carried forward:** `/server/auth/password_hash`.
- **Locked (identity) fields:** `rotator.unique_id`, `switch.unique_id`.
- **Hard read-only fields:** `server.port`, `rotator.enabled`, `switch.enabled`.
- **CLI-override-pinned:** `serial.port` (`--port`), `server.port` (`--server-port`).

A `config.apply` that changes a field persists atomically, returns
`status:"applying"`, and fires the in-process reload; `main.rs` runs under
`ServiceRunner::with_reload().run_with_reload(...)`, which tears the old server
down (releasing the shared serial port) and rebuilds from the freshly-persisted
file.

## Module Structure

```
services/pa-falcon-rotator/
├── src/
│   ├── lib.rs              # ServerBuilder, BoundServer
│   ├── main.rs             # CLI entry point; lifecycle owned by `rusty-photon-service-lifecycle::ServiceRunner`
│   ├── config.rs           # Config types + JSON loading
│   ├── error.rs            # FalconRotatorError (via rusty_photon_driver::driver_error!)
│   ├── rotator_device.rs   # ASCOM Device + Rotator trait impl
│   ├── switch_device.rs    # ASCOM Device + Switch trait impl (voltage + limit)
│   ├── codec.rs            # FalconCodec + FalconResponse + FalconCodecError
│   ├── serial.rs           # FalconTransportFactory (TransportFactory over tokio-serial)
│   ├── mock.rs             # MockFalconTransportFactory (feature = "mock")
│   ├── protocol.rs         # Command enum + response parsers + validate_echo
│   ├── manager.rs          # FalconManager wrapping SharedTransport<FalconCodec> + driver-side state
│   └── units.rs            # SkyDegrees / MechanicalDegrees / SyncOffset / Steps + frame algebra
├── tests/
│   ├── bdd.rs              # cucumber entry point (harness = false)
│   ├── bdd/
│   │   ├── world.rs        # FalconWorld
│   │   └── steps/
│   │       ├── mod.rs
│   │       ├── connection_steps.rs
│   │       ├── metadata_steps.rs
│   │       ├── position_steps.rs
│   │       ├── movement_steps.rs
│   │       ├── halt_steps.rs
│   │       ├── reverse_steps.rs
│   │       ├── sync_steps.rs
│   │       └── status_switch_steps.rs
│   ├── features/
│   │   ├── connection_lifecycle.feature
│   │   ├── metadata.feature
│   │   ├── position_reads.feature
│   │   ├── movement.feature
│   │   ├── halt.feature
│   │   ├── reverse.feature
│   │   ├── sync_offset.feature
│   │   └── status_switch.feature
│   ├── property_tests.rs         # proptest round-trip on FalconStatus wire format
│   └── conformu_integration.rs   # ASCOM Rotator + Switch conformance (#[ignore])
└── examples/
    ├── config-linux.json
    ├── config-macos.json
    └── config-windows.json
```

## Connection Lifecycle

1. Client `PUT /connected?Connected=true` on either device.
2. The device's `set_connected(true)` takes the device's session-slot write lock and (if the slot is currently empty) calls `transport().acquire()`.
3. `SharedTransport::acquire()` on the 0→1 transition opens the port via `FalconTransportFactory::open` (which wraps the `tokio-serial` stream in a `SerialFrameTransport` with `\n` framing) and runs the handshake hook atomically.
4. Handshake (sequential — `SharedTransport` rolls the refcount back and drops the transport on any error, so a half-connected state cannot escape):
   - `F#` → expect `FR_OK` ack.
   - `FV` → log firmware version at `info!`.
   - `DR:0` → force derotation off (known state regardless of how the device was last left).
   - `FA` → smoke-test the response shape (parsed but not stored — there is no cache).
   - `VS` → smoke-test the voltage response shape.
   - Initialises `last_limit_detected` to `None` so the first post-connect `read_status` observation triggers the rising-edge log if `limit_detect` is high.
5. The transport's `is_available()` flips to `true`; the device's session slot stores the returned `Session<FalconCodec>`.
6. Subsequent `set_connected(true)` calls on the *other* device (or repeated calls on the same device while connected) bump the shared refcount without re-running the handshake.
7. `set_connected(false)` takes the slot lock, calls `session.close().await`, and propagates any teardown error. On the 1→0 transition `SharedTransport` runs the (no-op for Falcon) teardown hook and drops the `FrameTransport`, closing the underlying serial port.
8. Once the transport reports `is_available() == false`, the driver-side `sync_offset` is reset to `0.0`, `target_position` is cleared, and the `last_limit_detected` edge tracker is reset by `FalconManager::clear_session_state`. None of these is persisted across reconnects.

## Move Lifecycle

For each of `Move(delta)`, `MoveAbsolute(skyDeg)`, `MoveMechanical(mechDeg)`:

1. Reject with `NOT_CONNECTED` if `connected() == false`.
2. For `Move(delta)`, the driver issues `FA` (through `FalconManager::read_status`) first to read the current `position_in_deg` so the relative move can be computed against the live mechanical angle. `MoveAbsolute` / `MoveMechanical` need no read.
3. Compute the mechanical target (per the [mapping table](#ascom-rotator-mapping)).
4. Quantise to the `MD:nn.nn` wire precision and normalise into `[0.0, 360.0)`.
5. Through the held `Session<FalconCodec>` (which holds the shared transport's command-arbitration lock for the call's duration), issue `MD:<wire>` and validate the echo (`MD:<wire>`).
6. Store the **wire-quantised** sky-coordinate target via `FalconManager::set_target_position` (a near-boundary input like `359.999` lands on the wire as `MD:0.00`, so the stored target tracks what the device was actually told to reach — not the raw input).
7. Return `Ok(())` to the client (ASCOM `Move*` returns immediately; the move is asynchronous).
8. Move completion is reported by `IsMoving` reads — each one issues a fresh `FA`. `target_position` survives completion so `TargetPosition` reads stay populated until either `Halt` or the next `Move*`.

`Halt()` issues `FH`, validates the `FH:1` echo, then clears `target_position`.

## Error Model

Matches the qhy-focuser / ppba-driver precedent for cross-driver consistency: only the error variants ASCOM clients actually distinguish on get their own codes; everything else collapses to `INVALID_OPERATION`.

| Driver error variant | ASCOM error code | Triggering conditions |
|---|---|---|
| `NotConnected` | `NOT_CONNECTED` (`0x407`) | Any property/method called while `connected() == false` |
| `InvalidValue(msg)` | `INVALID_VALUE` (`0x401`) | Client supplied `NaN` / non-finite angle to `Move*` / `Sync`; switch id ≥ `MaxSwitch` on any Switch method |
| n/a (Switch capability gap) | `NOT_IMPLEMENTED` (`0x400`) | `SetSwitch` / `SetSwitchValue` / `SetSwitchName` and the `SetAsync` / `SetAsyncValue` / `CancelAsync` writers on the read-only Status Switch device — see [Write surface](#write-surface-read-only-device) |
| All others | `INVALID_OPERATION` (`0x40B`) | `ConnectionFailed` (handshake failure), `SerialPort`, `Timeout`, `Io`, `InvalidResponse`, `ParseError`, `Communication` |

## MVP Scope

In scope for the first iteration:

- Connect / disconnect with handshake (`F# → FV → DR:0 → FA → VS`).
- `Position`, `MechanicalPosition`, `IsMoving`, `Reverse`, `TargetPosition` reads on the Rotator device — each backed by a live serial command.
- `MoveAbsolute`, `Move`, `MoveMechanical`, `Halt`.
- `Sync` as a driver-side offset.
- `Reverse` writes via `FN:b` with EEPROM-wear protection (read-then-write-if-different).
- Status Switch device: two read-only switches (voltage raw ADC + limit-hit boolean), each read on demand.
- ConformU compliance for both `IRotatorV4` and `ISwitchV3`.
- BDD scenarios mirroring each feature area listed in [Module Structure](#module-structure).

Deferred:

- De-rotation (no ASCOM mapping; design out a Custom Action or a derotator-flavoured device).
- Voltage raw-to-volts scale calibration (Switch currently reports raw ADC; scale factor or config-driven calibration is a follow-up).
- `SD`-based persistent sync (only if `Sync` offset persistence becomes a real requirement).
- Internationalisation of the CLI (`ppba-driver` style) — skip for MVP; reuse the spike outcome later.

## Testing

Per [`docs/skills/testing.md`](../skills/testing.md):

- **BDD** (cucumber-rs, primary): one feature file per concern in `tests/features/`. Scenarios run the `ServerBuilder` **in-process** on an ephemeral port and drive the registered devices through Alpaca HTTP clients (`AlpacaClient::get_devices`). The world holds an `Arc<MockFalconTransportFactory>` shared with the `SharedTransport`, which lets step bodies (a) seed mock state — reported mechanical position, voltage, `motor_reverse`, `limit_detect` — and (b) assert on the wire-level `command_log` for contracts like "F# is the first command" or "no SD was issued". The `tests/bdd.rs` entry point uses a plain `#[tokio::main]` rather than the `bdd_infra::bdd_main!` macro, because the macro is only needed for harnesses that spawn child processes via `ServiceHandle` (see [`testing.md` §5.2](../skills/testing.md#52-entry-point-structure)). Tag any in-flight scenario with `@wip` while its implementation lands; strip the tag in the same commit that turns the scenario green.
- **Unit tests** (`#[cfg(test)]` in `src/`): protocol serialisation, `FA` response parsing (happy path + every failure mode in the [error table](#error-model)), config defaults, sync-offset arithmetic, normalisation rules, and the `limit_detect` rising-edge rule (`is_limit_detect_rising_edge`, exhaustive over all six `previous × current` combinations). The `warn!` the edge drives is deliberately **not** asserted via a captured-event subscriber — see [`testing.md` §6.8](../skills/testing.md#68-do-not-assert-on-captured-tracing-events) and issue #949; the mock-backed tests assert the `last_limit_detected` tracker instead.
- **Property tests** (`proptest`): `FalconStatus` wire-format round-trip — serialise a randomly-generated status with `format!`, parse the result with `parse_full_status`, and assert field-by-field equality. Pins the parser against any future format-string drift.
- **Server tests** (`#[cfg(feature = "mock")]`, `tests/test_lib.rs`): bind the `ServerBuilder` on `127.0.0.1:0` with the `MockFalconTransportFactory` and assert the management surface — `/management/v1/description` responds, `/management/v1/configureddevices` lists both Rotator and Switch when enabled, and disabling `switch` registers only the Rotator. Tests run sequentially behind a `SERVER_LOCK` `Mutex<()>` so re-runs against a left-over discovery binding don't race.
- **ConformU** (`tests/conformu_integration.rs`, `#[ignore]`): run against the binary with the `mock` feature, mirroring `ppba-driver` / `qhy-focuser`. Registered under `[package.metadata.conformu]`.

## Follow-ups

These are explicit deferrals — not open design questions, but work the MVP intentionally leaves to a later iteration.

1. **Voltage scale calibration.** Switch `0` currently exposes the raw ADC count. Once the Falcon has been bench-measured against a known supply rail, replace the raw-count switch with a calibrated `Input Voltage (V)` switch using either a hard-coded scale or `voltage_scale` / `voltage_offset` config fields.
2. **ADC width.** Switch `0`'s `MaxSwitchValue = 1023` assumes a 10-bit ADC. If hardware characterisation finds a wider range (12-bit / 16-bit), widen the metadata in a follow-up PR.
3. **Sync offset persistence.** Driver-side `sync_offset` resets on every reconnect. If observatories ever ask for it to survive driver restarts (rare — the standard workflow is to re-plate-solve and re-sync each session), add a small JSON state file next to the config.
4. **De-rotation surface.** `DR` is forced off on connect and never exposed. If a real demand emerges, the natural designs are a Custom ASCOM `Action` ("StartDerotation"/"StopDerotation"), an additional read/write Switch, or a separate device alongside the existing two.
5. **Falcon Rotator v2.** This design targets the discontinued v1 hardware. If a v2 ever lands on this codebase, expect a fresh PDF + a partial protocol divergence.
6. **TLS-cert hot-reload.** Inherited from `ppba-driver` / `qhy-focuser`: the service requires a restart to pick up new TLS certs. Not a Falcon-specific issue.

## References

- **Pegasus Astro Falcon Serial Command Table** (firmware ≥ v1.3, Sep 2020): `https://pegasusastro.com/wp-content/uploads/2022/05/Falcon_Serial_Command_Table.pdf`
- **Falcon Rotator product page**: `https://pegasusastro.com/products/falcon-rotator/`
- **ASCOM IRotatorV4 spec**: <https://ascom-standards.org/newdocs/rotator.html>
- **`ppba-driver` design doc**: [`ppba-driver.md`](ppba-driver.md) — the architectural template (and the first `rusty-photon-shared-transport` consumer).
- **`qhy-focuser` design doc**: [`qhy-focuser.md`](qhy-focuser.md) — the closest behavioural analogue (single moving device with `IsMoving` + `Position`).

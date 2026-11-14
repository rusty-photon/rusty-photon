# qhy-camera: the delivery record

A record, moved whole from `docs/services/qhy-camera.md` on 2026-11-13: the
status banner the design document opened with, the phases the driver was
delivered in (June 2026), and the implementation notes that recorded, from
June to October 2026, how the build pinned down or departed from the design
as first written. The service had no plan in `docs/plans/`, so this is the
only account of its delivery. The banner stood at the top of
[qhy-camera.md](../../services/qhy-camera.md), and the two sections after
[§ Testing](../../services/qhy-camera.md#testing); the design document
states what the driver does today, and holds the notes' decisions that still
describe it under § Architecture, § MaxADU and § What a readout-mode change
is to the SDK.

## Status banner

> **Status:** Implemented (v0). The driver lives in
> [`services/qhy-camera`](../../../services/qhy-camera). All 10 BDD feature suites
> (73 scenarios) and the unit tests are green against the `qhyccd-rs`
> `simulation` backend; ConformU runs in CI. This document remains the
> behavioural specification — the handful of implementation deviations from the
> original design are called out inline (search "*Implementation note*"). The
> *Delivery phasing* § Phase 0–6 tracked the SDK-de-risk → full-driver rollout.

## Delivery phasing (E→C)

This service is built in two tracks to isolate the genuinely novel risk (the
proprietary system dependency) from the mechanical-but-large risk (the device
driver itself).

- **Phase 0 — decision gate** *(done)*. First-class managed device confirmed;
  enumerate-all device model; SDK pinned to **25.09.29**; arm64 confirmed.
- **Phase 1 — `ascom-alpaca` branch reconcile.** Land
  `fix/macos-trait-recursion-overflow` onto `integration` and repin upstream
  `qhyccd-alpaca` to `integration`, giving the fork one shared branch (fork
  hygiene — chosen even though it is not a compile-time prerequisite for this
  service under Option C, since `qhyccd-rs` carries no `ascom-alpaca` dep). A
  separate-repo operation on the `ascom-alpaca-rs` fork.
- **Phase 2 — Track A: isolate the system-dep risk.** Add `qhyccd-rs = "=0.1.9"`
  to `[workspace.dependencies]`. Stand up SDK (25.09.29) + `libusb` provisioning
  (CI step, `setup-pi-runner.sh` incl. arm64, Bazel `requires-cargo` tag, repin
  twice). Create a **bare `qhy-camera` exposing an ASCOM Camera in `simulation`
  mode on :11121** — proving build/link, CI, Pi5 arm64, and repin end-to-end
  **before** any device-trait work. *If the Bazel sys-crate path proves
  intractable, fall back to the `requires-cargo` carve-out (Cargo remains
  canonical); the camera still builds and runs under Cargo.*
- **Phase 3 — this design doc** *(done)* + the `docs/workspace.md` row.
- **Phase 4 — Track B: full driver (Option C, confirmed)** *(done)*. Implemented
  `Device + Camera` **and `+ FilterWheel`** natively against `qhyccd-rs`, using
  `qhyccd-alpaca`'s `main.rs` as the behavioural spec only (no vendored fork); a
  thin in-crate SDK seam (`backend.rs`) wraps the blocking `qhyccd-rs` handles so
  the device logic is unit-testable without hardware. Lifecycle, hardware-derived
  identity, and config-actions wired.
- **Phase 5 — test + gate** *(done)*. 8 BDD feature suites (56 scenarios) + unit
  tests green against the `simulation` backend; ConformU wired (skips without
  `CONFORMU_PATH`); `bazel build //...` + `bazel test //...` + `cargo fmt` + clippy
  green.
- **Phase 6 — consumer + Bazel finish** *(Bazel done; consumer pending)*. CI/Pi
  SDK provisioning landed. The `bdd` + `conformu_integration` Bazel targets are
  now **first-class** (no `requires-cargo`): they depend on the `testonly`
  simulated library `//crates/qhyccd-rs:qhyccd-rs_sim`, so they no longer call
  the real `InitQHYCCDResource` (see the Gating plan's Bazel row + ADR-009's
  first-party two-variant). Still pending: the `rp`
  `CameraConfig { alpaca_url: http://localhost:11121, device_number }` consumer.

## Implementation notes (v0 deviations from the original design)

Behaviour the implementation pins down or diverges from the design above. The
behavioural contracts and the BDD feature files remain the authority; these are
the "how" decisions made while building.

- **SDK seam (`backend.rs`).** The device structs hold an `Arc<dyn CameraHandle>`
  / `Arc<dyn FilterWheelHandle>` over a thin trait that wraps the blocking
  `qhyccd-rs` handles and collapses its `eyre::Report` into one typed error. A
  production wrapper drives the real SDK; a test mock lets the unit tests — incl.
  the E9 `Error`-state path and colour/shutter models the mono sim can't show —
  run with no hardware and no *real* SDK calls. (The static `qhyccd` lib is still
  linked into the test binary — that link is unconditional, see above; only the
  runtime seam is mocked.) The device logic reaches the well-known controls
  through **typed accessors** — `handle.gain()` / `set_gain(…)`,
  `handle.current_temperature_celsius()`, `set_target_temperature_celsius(…)`,
  `set_manual_cooler_pwm(…)`, `cooler_power_raw()`, `exposure_range_us()`, … —
  which the trait provides as defaults over the generic
  `get_parameter`/`set_parameter(ControlType, )` methods (mirroring
  `qhyccd_rs::Camera`'s own accessors; Phase 2 of the
  [convention-alignment plan](qhyccd-convention-alignment.md)). The
  generic pair stays for capability *probes* (`is_control_available`) and any
  control without a dedicated accessor. `qhyccd-rs`'s control enum is the
  `ControlType` subset (semantic variants + `Other(i32)`), not the SDK's full
  `CONTROL_ID` list.
- **MaxADU.** `2^bits − 1` where `bits` is the **transfer-container depth** from
  the cached `ccd_info.bits_per_pixel` (16 ⇒ 65535), defaulting to 16 for a
  camera that *reports* a depth of 0. Not for one whose depth nothing has read
  yet: while a connect's handshake is still running its cache is empty (C6), and
  defaulting there would answer 65535 on a model whose container turns out to be
  8 bits — a valid-looking number that changes under the client when the connect
  finishes — so an unread depth is `VALUE_NOT_SET`.
  It is **not** `OutputDataActualBits`: the driver sets a 16-bit container at
  connect (`set_transfer_bit_16`) and the SDK left-shifts each raw sensor reading
  to fill it (zero-padding the low bits — SDK manual §14), so a client receives
  values up to the container max regardless of the sensor's native ADC depth.
  Confirmed on hardware: the 12-bit IMX290 returns values quantised in steps of 16
  up to 0xFFF0, and `OutputDataActualBits` reads 14 (IMX178) / 12 (IMX290) / **0**
  (QHY5III715C) — the last of which made the old `2^OutputDataActualBits − 1`
  formula yield `MaxADU = 0` (ConformU: "below minimum"). The container-depth
  formula is uniform across all models and never 0.
- **Dark frames** → `NOT_IMPLEMENTED` on all models (E4) — no shutter actuation
  in `qhyccd-rs` 0.1.9.
- **FilterWheel `UniqueID`** is `CFW-<sdk-id>` (prefixed), because a `qhyccd-rs`
  `FilterWheel` delegates `id()` to its underlying camera and would otherwise
  collide with the camera's `UniqueID` on single-handle models.
- **Empty simulation backend** (the C0 zero-camera scenario) is selected by a
  hidden, `simulation`-feature-gated `--simulation-empty` CLI flag that makes
  `build()` use `Sdk::new_simulated()` (empty) instead of `Sdk::new()`.
- **Transport.** v0 serves with plain `axum::serve` on `server.port`. Alpaca
  UDP discovery is opt-in via `server.discovery_port` (absent by default —
  many rusty-photon servers on one host would collide on the shared
  discovery port), like every Alpaca service.
  The listener is created via the shared `rusty_photon_tls::server::bind_dual_stack_tokio`
  helper (IPv6 + IPv4, `SO_REUSEADDR`) like every other Alpaca service, so the
  in-process `with_reload` rebind survives a prior listener's lingering
  `TIME_WAIT`. TLS termination / Basic Auth (the rest of `rusty-photon-tls` / `rp-auth`)
  are still Future Work.
- **A cancel may never race a readout.** `qhyccd.h` documents
  `CancelQHYCCDExposingAndReadout` as *"the camera does not send back the image
  data. Host software must not readout the data"*, so the SDK cancel and
  `GetQHYCCDSingleFrame` must never overlap. The capture task is therefore split
  into three phases — start, a **cancellable wait** for the exposure to elapse,
  and an **uninterruptible readout** — and an abort is honoured only between
  them. `cancel_exposure` signals the in-flight capture's own cancel channel
  (which both raises its flag and wakes it), waits for that capture to leave the
  SDK, and *only then* issues the SDK cancel. An abort taken during the exposure
  skips the readout entirely; one
  taken during the readout waits for it to finish, after which the cancel is the
  same harmless pre-close reset the SDK's own `SingleFrameSample` performs.
  indi-qhy keeps exactly this discipline (its `AbortExposure` blocks on the
  imaging thread leaving `StateExposure` before calling the SDK cancel), and the
  SDK's samples never cancel concurrently with a readout either. Getting this
  wrong is not merely untidy: it leaves `GetQHYCCDSingleFrame` waiting on image
  data the camera has been told never to send.
- **Waiting for the exposure without holding the SDK.** The wait is host-side
  first (a 30-minute exposure costs no USB traffic), then
  `GetQHYCCDExposureRemaining` must agree the exposure is over before the readout
  is entered — if the host clock ran ahead, `get_single_frame` would block inside
  the readout for the remainder, re-opening the window the split exists to close.
  Polling is capped at `EXPOSURE_POLL_INTERVAL` and the confirmation phase at
  `EXPOSURE_CONFIRM_TIMEOUT`, after which the readout is entered anyway so a
  camera that never reports 0 cannot strand the frame. A failed poll enters the
  readout too, unless the C9 question it asks finds the camera gone; a departed
  camera is not read out. A cancel never waits for
  a poll: the capture's cancel channel wakes the sleep immediately.
- **SDK call serialization — the claim *is* the cancel channel.** The single
  in-flight capture is the one logical owner of the device's blocking SDK calls.
  `start_exposure` claims the device by installing that capture's own cancel
  channel in `in_flight_capture`: `Some` **is** the claim, so a device that
  reports itself exposing always has something an abort can signal. A capture is
  the usual holder but not the only one — a disconnect's close, an abort's SDK
  cancel and a readout-mode change (B4) each take a claim of their own, on the same
  terms: while it is installed, that holder and nothing else may be inside the
  SDK. Holding the
  two apart — an `AtomicBool` claim taken first, a handle-wide cancel flag
  cleared a statement later — leaves a window in which an abort is *erased* by
  the exposure that admitted it, and the client then waits out the drain deadline
  for an `AbortExposure` that cancelled nothing. Because the channel is per
  capture rather than per device, no exposure can clear another's cancel, and an
  abort signals the capture it was issued against and no other.

  `cancel_exposure` (abort/disconnect) bumps a generation and signals that claim
  but does **not** release it — the capture task takes it back only after its SDK
  calls have fully drained, so a new exposure cannot start and race them, and
  only the installer of a claim ever takes it back. A reconnect's
  `reset_exposure_state` is the one place that could be tempted to take another
  owner's claim and deliberately does not: here the claim means *something is
  inside the SDK*, so handing the device on while that is still true is exactly
  what would let an SDK cancel land on a live readout. It signals instead, and a
  `StartExposure` in that window is rejected rather than started alongside.

  A short `result_lock` covers every transition of this state machine: the
  generation bump, the claim install and take, and the capture task's "check
  generation + commit result". So an abort reads the claim and bumps the
  generation knowing no start, drain or reconnect can slip between the two, a
  just-completing capture can never resurrect an aborted frame, and a successor
  exposure cannot lose its frame to a bump meant for its predecessor.

  The drain is **event-driven on a deadline, not a
  polling sleep**, and it waits for the *specific* claim the abort signalled
  rather than for the device to fall idle — an abort whose target has already
  been superseded must not sit out the successor's exposure and then report a
  failure belonging to neither. The capture task fires a `tokio::sync::Notify`
  (`exposure_drained`) the instant the claim leaves, and the waiter awaits it
  under a single `tokio::time::timeout` (canonical `Notified` `enable()`-before-
  check pattern, so a release landing between the check and the await is never
  lost). Earlier this was a `loop { sleep(5 ms) }` busy-wait, replaced because
  repeated short sleeps can stall under scheduler pressure.
- **A stuck SDK call blocks the close rather than being closed through.**
  `disconnect` closes the handle only once the capture task is out of the SDK.
  Closing it under a live USB transfer frees the handle beneath libusb — a
  use-after-free that trips its `usbi_mutex_lock` assertion and can corrupt the
  SDK's shared libusb context. If the drain deadline (`CAPTURE_DRAIN_TIMEOUT`,
  30 s — sized for a readout, since the exposure wait is cancellable) expires,
  `disconnect` issues **no** SDK cancel, leaves the handle **open**, and returns
  an error. A failed disconnect is the lesser evil, and it reports the stuck
  device honestly instead of hiding it behind a close that may corrupt state.
  indi-qhy takes the same position more bluntly, with an unconditional
  `pthread_join` before its `CloseQHYCCD`.

  **Draining is not enough on its own — `disconnect` has to keep the device.**
  Every drain ends with the device *unclaimed*, which is exactly the state a
  `StartExposure` is waiting for: it can claim, push its ROI and exposure, and
  be inside `GetQHYCCDSingleFrame` before the close lands. So the drain and the
  close are one critical section from the ownership point of view, and
  `disconnect` holds a claim of its own across both, releasing it only after
  `close()` has returned (also when `close()` *fails*, so a refused close cannot
  wedge the device claimed forever). While that claim is installed a racing
  `StartExposure` is refused — by the connected check once
  `SharedCameraConnection` has cleared the flag, and by the ordinary E2 path in
  the window before that. The claim is what makes the close safe rather than
  merely likely to be safe: it owns the device even where the flag is still set.

  **A section that owns the device runs where cancellation cannot reach it.**
  Every SDK call runs off the executor, so each path that owns the device —
  `StartExposure`'s arming, `disconnect`'s seize-and-close, and `AbortExposure`'s
  drain-and-cancel — holds its claim across an `.await`. An Alpaca client
  disconnecting mid-request is enough for the server to drop that future, and
  neither answer available to a plain `.await` is safe:

  - *Never release.* No code of ours runs after the drop and nothing else can
    release a claim on its behalf, so it stays installed for the life of the
    process — every later exposure refused as already-exposing, every later
    disconnect a drain that never completes.
  - *Release immediately.* The `spawn_blocking` call the future was awaiting is
    **not** cancelled with it and is still inside the SDK. Handing the device
    back then lets a successor claim it and issue calls that overlap the orphan,
    and nothing below stops them: `qhyccd-rs` guards the handle with a *read*
    lock that admits concurrent non-close calls by design. An SDK cancel
    overlapping a readout is precisely what `qhyccd.h` forbids.

  So these sections are not run in the request future at all. Each is spawned as
  its own task and the request awaits its `JoinHandle`; dropping a `JoinHandle`
  detaches the task rather than stopping it, so the section always runs to
  completion and gives the device back only once its SDK call has returned.
  Within a section, a `Drop` guard covers the ordinary and error exits so there
  is no second release to keep in step — except `StartExposure`'s success, which
  hands the claim to the capture task instead.

  The claim is what makes the *shutdown* orderly — a `StartExposure` racing the
  close is refused rather than started and then torn down, and an operator gets
  a reported failure instead of a device that closed under a live transfer.
  Beneath it, `qhyccd-rs` holds its handle's read lock across every FFI call
  (`HandleCell::with_handle`), so a close waits for anything in flight rather
  than freeing the handle under it — the same guarantee `zwo-camera` and
  `svbony-camera` get from backends that hold their handle mutex across the call
  and close by clearing that same slot.

  The two are not redundant, and the difference is worth keeping straight when
  changing either. The lock cannot express the SDK's *ordering* rules — a cancel
  and a readout are both read guards, and `qhyccd.h` forbids overlapping them —
  and on its own it would turn a wedged readout into an unbounded block on the
  close rather than the reported refusal above. It also covers far more than the
  capture path: every property read reaches the SDK outside any claim, because a
  temperature poll is not a capture and must not be refused during one. So the
  claim decides *when* a close may be attempted, and the lock guarantees that
  once attempted it cannot land underneath a call — anyone's call, not just a
  capture's.

  **A disconnect wins over an exposure that starts during it.** When the drain
  ends and the device has already been re-claimed by a new capture, `disconnect`
  drains that one too and keeps going until it holds the device or the deadline
  expires — the operator asked for the device to go away, and a shutdown that
  cannot complete at an unattended rig costs more than the frame does. The
  deadline is a total budget across all rounds, not per round, so a client
  starting exposures in a loop cannot stall a disconnect indefinitely; it exits
  through the same refuse-to-close path a stuck readout takes, but is reported
  apart from it — the two ask different things of an operator, since an SDK that
  never came back usually means power-cycling the camera while a lost race just
  means retrying the disconnect. (`AbortExposure`
  keeps the opposite rule — E7: it cancels the capture it was issued against and
  no other, so finding the device re-claimed means its target is already gone
  and it returns `OK`.) The alternative considered and rejected was clearing the
  device's logical `connected` flag *in the device layer*, ahead of the seize, so
  racing `StartExposure`s bounce on `NOT_CONNECTED` and there is no contest at
  all: cleaner there, but `SharedCameraConnection::connect` reads that flag and
  takes its refcount in one critical section, so clearing it outside that
  section without dropping the ref lets a concurrent connect take a second ref
  and leak the physical handle open. That is a change to the one invariant in
  this service with a dedicated concurrency test, for a race the claim already
  closes. `disconnect` does clear the flag before `CloseQHYCCD`, but *inside*
  that critical section — which is what makes it safe there, and why a racing
  request sees `NOT_CONNECTED` for most of the close regardless.
- **Camera + CFW share one physical handle — refcounted shared connection.**
  `qhyccd-rs` derives the CFW from the *same* camera id as the enumerated camera
  (a QHY CFW is driven over the camera's USB, not a separate device). The SDK
  keys the open device by id, so opening that id as two independent handles and
  closing either one tears down the shared physical device and breaks the other.
  This was **confirmed on real hardware (QHY178M + 7-slot CFW, 2026-06-18):**
  disconnecting the CFW made the next camera `StartExposure` fail with
  `SetRoiError` (QHYCCD_ERROR), and disconnecting the camera made CFW `Position`
  fail with `INVALID_OPERATION` — and in both cases the still-"connected" device's
  `is_open()` kept (mis)reporting `true`. The Camera and FilterWheel devices
  therefore share ONE
  [`SharedCameraConnection`](../../../services/qhy-camera/src/backend.rs): one
  `qhyccd-rs::Camera` (the CFW operates a clone that shares the same internal
  handle `Arc`) behind a refcount of logical connections — the physical
  `OpenQHYCCD` runs on the first device's connect and `CloseQHYCCD` only on the
  last device's disconnect, while each device keeps its own logical `connected`
  flag so its ASCOM `Connected` reflects that device, not the shared handle.
  Validated end-to-end on hardware (disconnect-CFW-then-expose and
  disconnect-camera-then-move-CFW both succeed) plus unit tests over the
  simulation backend (`backend::conn_tests`). *This supersedes the v0 plan, which
  used independent handles "as the reference `qhyccd-alpaca` does" and deferred
  the refcount as Future Work pending hardware.* Since the `qhyccd-rs` **Phase-1
  handle-model alignment** ([qhyccd-convention-alignment.md](qhyccd-convention-alignment.md)),
  the crate itself shares one handle cell between a camera and its filter wheel
  and closes it on last-drop (RAII), and `Sdk::drop` closes every open camera
  handle **before** `ReleaseQHYCCDResource` (the SDK-documented Close-then-Release
  order). So a device still Connected at process shutdown or reload is now torn
  down cleanly instead of leaked; the service's own `SharedCameraConnection`
  refcount and per-device `Connected` semantics are unchanged.
- **Cooling model.** v0 had `set_cooler_on(true)` engage a nominal 1% *manual*
  PWM (matching the reference), distinct from the automatic target-temperature
  regulation `SetCCDTemperature` drives — a real ASCOM client sequence of
  `SetCCDTemperature` then `CoolerOn(true)` left the cooler pinned near 1%
  power (confirmed on real hardware). `set_cooler_on(true)` now
  re-asserts the cooler target (`ControlType::Cooler`, via
  `set_target_temperature_celsius`) with the stored target instead; see
  [Cooling contract K4](../../services/qhy-camera.md#cooling).
- **What a readout-mode change is to the SDK.** Read from the shipped
  `libqhyccd` (26.06.04 and 25.09.29, Linux x86_64) by static disassembly —
  nothing was run against a camera — and backing RM1, RM3 and RM4:
  - `SetQHYCCDReadMode` dispatches to the camera class's `SetReadMode`, which on
    the QHY600 class records the index in the SDK object and returns: no USB
    transfer at all. The mode reaches the camera only in the class's
    `InitChipRegs`, which `InitQHYCCD` calls, as an argument to the FPGA init
    command; the same routine writes the SDK's per-mode chip size, effective
    area and overscan. Every multi-mode class in the library reads the mode in
    `InitChipRegs` and none sends it from `SetReadMode`.
  - A mode change **without** `InitQHYCCD` is therefore half-applied: the SDK's
    resolution, bin and gain logic switch to the new index at once while the
    camera stays in the old mode, and the chip info and effective area keep the
    old mode's values. The driver's previous `set_readout_mode` did exactly that;
    it never ran on hardware, because every validated QHY camera (the QHY178M)
    has one mode, whose base-class `SetReadMode` accepts 0 and nothing else.
  - The bin is different, which is why the vendor manual's §15 — one procedure
    for readout mode, bin and data format — over-generalizes: `SetChipBinMode`
    rebuilds the effective area and overscan itself, with no init, and a bare
    `SetQHYCCDBinMode` is hardware-validated (R4) — sent, as `StartExposure`
    sends it, just ahead of the region (B1).
  - `SetQHYCCDStreamMode` does not read the recorded mode either (no class's
    stream-mode code refers to it), so the order of the stream mode and the
    read mode ahead of an init does not matter to the SDK; the driver sends
    them in connect's order.
  - `InitQHYCCD` returns success whatever `InitChipRegs` returns, and nothing
    the SDK reports afterwards distinguishes the two: `GetQHYCCDReadMode`
    returns the index `SetReadMode` recorded, which only the constructors and
    `SetReadMode` write, and the chip info and effective area are whatever
    `InitChipRegs` got as far as writing. RM3's checks therefore catch an
    unrecorded mode and an empty readable area, not a failed init. The init
    resets the QHY600's exposure to 5 s and transfer depth to 16
    (8 in live mode), re-sends the SDK's stored gain, offset and USB traffic,
    and forces DDR on; it starts one more sensor-status thread per call (the
    SDK's own, until the handle closes). When `qhyccd.ini` sets
    `disable_auto_cooler=true` it sets the cooler PWM to 0 on every call. Its
    single-frame path on the QHY600 sleeps 0.4 s.
  - The QHY600 class reports **11** modes, all 9600x6422 except mode 5
    (`Bin3*3Mode (hardware)`, 3200x2144), and four of them named
    `(Fiber Only)`; the SDK does not filter those out over USB. Mode count,
    names and per-mode resolution are static tables, readable without switching.
    `GetQHYCCDReadModeResolution` is not safe to call per index on every model —
    on the QHY294PRO it rewrites the SDK's margin state as a side effect — so the
    driver sizes a mode from chip info read after the switch, as connect does,
    and never from that call.

  Other drivers split the same way: INDI and AlpacaBridge re-initialize at the
  setter, N.I.N.A. switches inside `StartExposure` with a re-init, and INDIGO
  and several smaller drivers call `SetQHYCCDReadMode` alone. The switch has
  since been run on a multi-mode camera on Windows (RM1, *Measured on
  hardware*), and what that run could see agrees with this reading — the new
  mode's geometry after the init, one more SDK thread per init until the close,
  the mode list and mode 5's size — but the Windows `qhyccd.dll` itself has not
  been read.

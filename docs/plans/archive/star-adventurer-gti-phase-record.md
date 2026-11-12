# star-adventurer-gti: the phases the driver was built in

A record, moved whole from `docs/services/star-adventurer-gti.md` on
2026-11-12: the status table of the phases the driver was delivered
in, what the first connect to a physical Star Adventurer GTi showed
about the wire protocol, and the driver-logic changes that real
hardware required. It stood under
[star-adventurer-gti.md § MVP Scope](../../services/star-adventurer-gti.md#mvp-scope);
"this document" in the table's first row is that design document,
which states what the driver does today.

## Phase status

| Phase | Status |
|---|---|
| **Phase 1 — Design doc** | ✓ landed (this document, PR #178) |
| **Phase 2 — BDD scaffold** | ✓ landed: codec crate skeleton, service skeleton, all feature files (`@wip`), step stubs (PR #180) |
| **Phase 3 — Implementation** | ✓ landed: codec, transports (USB+UDP), `MountDevice`, ConformU integration (PR #188); BDD step bodies + `@wip` removal (PR #189). All 9 feature files / 77 scenarios green on Linux/Windows/macOS CI. |
| **Phase 4 — Real-hardware bringup** | partially landed — first hardware connect surfaced several protocol-decoding gaps that the mock had hidden. Details below. |
| **Phase A5 — `:I`/`:M` on slew + EQMOD pickup** | landed (issue #205) — reinstates `:I` on the slew path, switches goto to `:H` (delta target) + `:M` (break-point), and adds an iterative post-stop pickup loop to close the residual RA drift the Phase 4 ConformU run flagged. |
| **Phase A6 — Dec-encoder `SideOfPier` + `DestinationSideOfPier`** | landed (issue #202) — switches `SideOfPier` from the RA mech-HA split at `HA = 0` to the canonical INDI eqmod Dec-encoder convention (`East` when `\|dec_encoder\| > cpr_dec/4`), and lands `DestinationSideOfPier` reusing the same coordinate-math pipeline as `SlewToCoordinatesAsync`. ConformU expected-issues count moves from 9 to 7: the `DestinationSideOfPier` NotImplemented entry and the four inherited `SOPPierTest` entries clear, the two upstream `TrackingRate Write` entries disappear (unrelated framework fix), and three new "non-flipping mount" entries appear that reflect ConformU's flip-aware-GEM assumption rather than driver bugs. See [§Expected ConformU report](../../services/star-adventurer-gti.md#expected-conformu-report). |
| **Phase A7 — PulseGuide** | landed (issue #206) — implements `PulseGuide` as a rate-shifted tracking burst on the targeted axis (no `:P`; that's the ST4-jack rate setter, not a pulse trigger), flips `CanPulseGuide` and `CanSetGuideRates` to `true`. Re-enabled `[package.metadata.conformu]` so the full two-phase ConformU integration ran again — but the `conformance` phase, which `alpacaprotocol`-only manual runs hadn't exercised, surfaced three failures that PR #206's review hadn't caught; see Phase A8. |
| **Phase A8 — Nightly ConformU opt-out (#201)** | landed (issue #201) — removed `[package.metadata.conformu]` again. ConformU's `conformance` phase fails for three independent reasons that need driver work first: (a) `SideOfPierTests` slews to mech-HA ±9 h, which the safety envelope correctly rejects on hardware but which ConformU treats as a fatal CheckMethods-level exception that abandons the rest of the suite; (b) `SideOfPier` always returns `pierWest` for in-envelope targets (Dec-encoder convention) where ConformU asserts the ASCOM pointing-state convention; (c) PulseGuide Dec moves at full sidereal rate instead of `guide_rate_dec_fraction × sidereal`. See [§Running ConformU manually](../../services/star-adventurer-gti.md#running-conformu-manually) and [§Expected ConformU report](../../services/star-adventurer-gti.md#expected-conformu-report) for the failure details and reproduction steps. |
| **Phase 5 — user-defined `SetPark` + persistence** | landed (issue #203) — park target now sourced from `mount.park_ra_ticks` / `mount.park_dec_ticks` in the config (fallback: encoder positions captured at handshake), `SetPark` writes the current encoder pair back into the running config file via atomic rename, `CanSetPark` flips on when `--config` is provided. See [§Park lifecycle](../../services/star-adventurer-gti.md#park-lifecycle) and [§Park persistence](../../services/star-adventurer-gti.md#park-persistence). |
| **Phase 6 — meridian-flip support** | hardware-validated 2026-05-16 (lat 32.7°N) — adds meridian flips — opt-in at first behind a `MountConfig::flip_policy.enabled` switch (and a `flip_range_hours` window removed again in 2026-09), always on since 2026-10 (see [§Flip policy](../../services/star-adventurer-gti.md#flip-policy)) — the asymmetric CW exclusion zone safety envelope, CW-exclusion zone-path-aware through-wrap RA routing, visible-pole Dec routing, `SetSideOfPier`, and flip-aware `DestinationSideOfPier`. End-to-end AP Park 1–5 traversal (including the through-wrap saddle-east flip and its flip-back) ran clean; the flip-back from the saddle-east wrap caught a sign-blind heuristic in `flip_slew_ra_delta` that the path-aware check now handles. The tracking-time CW-exclusion-zone safety guard (Part 1 of issue #259) has since landed — a background watcher stops tracking before the encoder `mech_HA` drifts into the zone (see [§Tracking-time safety guard](../../services/star-adventurer-gti.md#tracking-time-safety-guard)). Driver-planned auto-flip-during-tracking (Phase 2.5 / Part 2 of #259) has since landed as well — the same watcher can start a standard flip on the operator's behalf once tracking carries `mech_HA` past a configured offset, opt-in via `auto_flip.enabled` and pending its own real-hardware validation (see [§Auto-flip during tracking](../../services/star-adventurer-gti.md#auto-flip-during-tracking)). Plan: [`docs/plans/archive/star-adventurer-gti-meridian-flip.md`](star-adventurer-gti-meridian-flip.md). See [§Meridian flip](../../services/star-adventurer-gti.md#meridian-flip). |
| **Phase 7 — altitude-based safety floor (#223)** | landed 2026-07-01 ("Phase 3" in the plan's local numbering) — replaces the rectangular celestial-Dec envelope (`dec_limits`) with `MountConfig::min_altitude_degrees`: slew / sync targets whose computed apparent altitude (`sin alt = sin lat · sin dec + cos lat · cos dec · cos HA`) is below the floor are rejected with `INVALID_VALUE`. Default `0.0` (geometric horizon); negative floors permit below-horizon pointing and log `info!` at startup. `Park` stays exempt (privileged-park pattern). See [§Altitude floor](../../services/star-adventurer-gti.md#altitude-floor). |

### Phase 4 findings (hardware bringup)

First connecting the driver to a physical Star Adventurer GTi
revealed four wire-protocol issues the mock had not been
exercising. All are now patched, with regression tests in the
protocol crate and the BDD suite.

1. **`:g<axis>` payload width.** The spec is ambiguous about
   payload width for `Inquire High-Speed Ratio`. Real GTi returns
   a **2-hex-byte** payload (`=01\r`) on both axes — not the
   6-hex-byte u24 that the codec originally assumed. The codec
   now accepts both widths; the value (`0x01`) is stored as a
   plain `u32` in the parameter cache. Note that the documented
   "16, 32, 64" expected high-speed-ratio values do **not** match
   what this firmware returns. The driver no longer relies on
   the high-speed-ratio for slew-rate computation — see point 3.

2. **`!XX\r` error frame width.** Spec §4 documents a 2-hex-digit
   error code; empirical GTi returns a **1-hex-digit** form
   (`!4\r`) for the single-digit codes defined in §5. The codec
   accepts both 3- and 4-byte error frames.

3. **`:G` mode-byte semantics — most damaging bug.** The `:G`
   payload is **two independent hex nibbles** (DB1 then DB2 per
   spec §5), each with its own bit assignments. The original
   codec treated the byte as a flat 8-bit bitfield with
   `goto = 0x10, fast = 0x20, reverse = 0x01`. By coincidence the
   wire bytes it produced for `GOTO_FAST_FORWARD` (`"30"`) decode
   under the spec as **Tracking-Fast-CW**, which never auto-stops
   at the `:S` target. Every slew the driver issued was
   effectively a continuous-step command. The codec was rewritten
   to encode each nibble correctly:
   - `MotionMode::TRACKING` → wire `"10"` (DB1=1 Tracking, DB2=0 CW)
   - `MotionMode::GOTO_FAST_FORWARD` → wire `"00"` (DB1=0 Goto+Fast, DB2=0 CW)
   - Reverse direction flips DB2 bit 0 (e.g. `"01"` / `"11"`).

4. **`:f` status nibble-0 bit-1** decoded as "Forward" in the
   original codec. Per spec it is **CCW**. Renamed
   `AxisStatus.forward` → `AxisStatus.ccw`; `AxisStatus.blocked`
   and `AxisStatus.level_switch_on` (spec nibble-1 bit-1 and
   nibble-2 bit-1, respectively) added at the same time. The
   slew watcher now aborts on `blocked`.

### Phase 4 driver-logic changes that real hardware required

In addition to the codec fixes:

- **`stop_and_wait`** — `:K` (decelerate) only *requests* a stop;
  the motor takes meaningful wallclock time to actually halt.
  `:G` against a still-decelerating axis returns `!2\r`
  (`MotorNotStopped`). The slew, park, and `set_tracking(true)`
  paths issue `:K` (decelerate) and then poll `:f` until
  `running == false`, before any subsequent `:G`/`:S`/`:J`. An
  early version of this path used `:L` (instant stop) instead;
  switched to `:K` in issue #207 to match the spec's recommended
  stop semantics and INDI eqmod's `StopWaitMotor`
  (`indi-eqmod/skywatcher.cpp:1741-1765`). `:L` is kept for
  genuine emergency stops (`AbortSlew`, slew/park watcher abort
  on `blocked`). The spec presents it as the harsher stop, but
  from goto speed the GTi decelerates identically under both, so
  on this mount the choice records intent rather than load (see
  [§Safety stop at startup](../../services/star-adventurer-gti.md#safety-stop-at-startup)).
  Mock hid the wallclock issue originally because it processes
  `:K`/`:L` instantaneously.
- **No mode-cache short-circuit (attempted and reverted)** — issue
  #207's initial plan included an INDI-style
  `LastRunningStatus == NewStatus` cache to skip `stop_and_wait +
  :G` when the requested mode matched the last one we acked. **The
  implementation we tried did not work on real hardware.**
  Mock-mode ConformU + the unit/BDD suites all passed cleanly, but
  the first ConformU run against the physical GTi triggered an
  unbounded Dec slew: the cache said `Goto-Fast-CCW` after a `:E`
  (sync), but the firmware-side mode had drifted; the resulting
  `:I/:H/:M/:J` started a slew that ran ~360° of unwanted Dec
  motion before the pickup loop fired a ~269° corrective slew back.
  See PR #210 for the wire trace. The cache was reverted to an
  unconditional `stop_and_wait + :G` on every slew/park/tracking
  prep — the spec-recommended sequence and what INDI's
  `StopWaitMotor` does internally.

  Two reasons not to retry naively if/when revisiting:

  1. **`:E` (sync) is one observed mode-state desync, but not
     necessarily the only one.** Tracking transitions, post-goto
     auto-engage on RA, `:L` aborts, blocked-axis recovery, and any
     other state-changing command can shift firmware-side mode
     without an explicit `:G` from us. A cache that mirrors only
     what we *issued* is structurally unsafe.
  2. **UDP transport widens the failure mode.** The wire protocol
     is identical on USB-CDC and UDP/11880, but UDP can drop,
     reorder, or duplicate packets without us noticing; a cache
     that records "we sent `:G CCW` and saw an ack" can lie even
     more easily if a `:G` response was actually a stale duplicate
     ack from a prior frame. Anything we cache about firmware state
     must survive both half of the codec layer being lossy.

  Any future cache implementation needs a **background validation
  loop** that re-reads the actual `:f` mode bits (per spec §5, the
  status nibble reports the live mode) often enough to catch
  desyncs *before* the next `:G`-eliding op fires — polling cadence
  needs to be comparable to or faster than the smallest slew a
  caller could chain, and a desync detection must invalidate the
  per-axis cache immediately and refuse short-circuits until the
  next confirmed `:G` ack. Without that — or without a different
  approach that doesn't depend on a snapshot of state we don't own —
  the cache is structurally unsound.

  The live-rate guide pulse is not such a cache, and does not reopen
  this. It skips `:K` / `:G` only on the strength of a live `:f1` read
  one round trip earlier, and sends nothing but `:I1`, which cannot
  start motion or change direction. The driver's own memory can only
  veto it. See "The gate reads the wire, and the driver's memory can
  only say no" under [§PulseGuide lifecycle](../../services/star-adventurer-gti.md#pulseguide-lifecycle).
- **INDI-style slew sequence** — Phase A5 reinstated `:I` on the
  slew path and switched the goto from `:S` (absolute target) to
  `:H` (delta target) plus `:M` (break-point increment), matching
  what `indi-eqmod`'s `SlewTo` emits. The spec §3 phrasing that
  "the firmware picks goto speed internally" turned out to be
  half the story: the firmware does, but only when `:I` has
  primed `minperiods[axis]` (INDI defaults to `6`). Without it
  the goto runs at a slower step period and the deceleration
  ramp set by `:M breaks = min(|delta|/10, 3200)` is missing,
  which is what produced the post-stop residual RA drift the
  Phase 4 ConformU run flagged. Park still uses `:S` since its
  target is encoder 0 and the absolute form is the simpler fit.
- **Pickup-loop accuracy stack** — issue #207 follow-up; three
  layered fixes that together brought real-hardware ConformU
  residuals from mean 8.4″ / max 11.0″ (2 × 10″ tolerance
  crossings) down to mean 2.4″ / max 6.6″ on USB and mean 5.0″ /
  max 8.7″ on UDP, with **zero crossings on either transport**.

  1. **LST pre-compensation** —
     [`coordinates::pickup_target_ra_ticks`] computes each
     iteration's corrective encoder target for `LST(now +
     projection)` rather than `LST(now)`. Without it the slew
     lands at where the target *was* one iteration ago, and the
     residual floor matches the per-iteration LST drift (~6″ on
     USB, ~14″ on UDP).

  2. **Adaptive projection** — the watcher tracks the wall-clock
     interval between consecutive pickup decisions and uses it as
     the projection for the next iteration. Self-tunes per
     transport without per-transport config: USB iterations
     stabilise at ~400 ms, UDP at ~1100 ms. First iteration (no
     prior data) falls back to `polling_interval × 2`.

  3. **Pause background polling during the slew** — the watcher
     acquires a [`MountManager::pause_background_polling`]
     RAII guard at the top of its spawn. While paused, the
     watcher owns the wire: pickup wire commands (`:K :G :I :H
     :M :J`) fire without contending with `:j` / `:f` polls for
     the shared-transport command lock, and the watcher's
     [`MountManager::poll_axes_now`] drives the snapshot's
     freshness with one wire round-trip per loop iteration. The
     guard is advisory rather than synchronous: it bumps a depth
     counter that the poll loop reads once per iteration, at the
     top, so a poll cycle already past that read runs to
     completion and can still put a full cycle of `:j` / `:f`
     frames on the wire after the guard is taken. That is harmless —
     the shared-transport command lock serialises them against
     the watcher's own commands, and the guard exists to shed
     wire load, not to provide mutual exclusion. The
     guard is dropped explicitly right after tracking restart,
     before the settle delay — so background polling resumes
     during settle and the snapshot reflects the actively-tracking
     encoder position by the time an Alpaca client reads
     `RightAscension` post-`Slewing`. Releasing the guard early
     vs. on watcher exit makes a measurable difference (UDP mean
     7.3″ → 5.0″ in the experiment runs). Park watcher follows
     the same pattern.

  See `docs/plans/archive/star-adventurer-gti-pickup-accuracy.md` for the
  experiment plan and the diagnostic data that drove these
  choices.
- **Mechanical safety envelope** — driving the mount into the
  counterweight-up region with ConformU's pier-flip tests stalled
  the motor against a hard stop while the encoder counter kept
  advancing (audible motor noise, OTA stationary). `MountConfig`
  carries the asymmetric mech_HA interval where the CW binds against
  the pier (`cw_exclusion_zone`) plus the apparent-altitude floor
  (`min_altitude_degrees`, which replaced the rectangular `dec_limits`
  envelope 2026-07-01) —
  both wrapped as validated newtypes per
  [ADR-006](../../decisions/006-typed-physical-quantities-for-mount-pointing.md).
  `SyncToCoordinates` and
  `SlewToCoordinatesAsync` reject targets whose chosen-side
  `mech_HA` falls inside the CW exclusion zone with `INVALID_VALUE`
  before any wire motion; `Park` writes the target encoder ticks
  directly without the CW-exclusion zone check, matching INDI EQMOD's
  privileged-park pattern. Pre-Phase-6 builds used a symmetric
  natural-side window (`ra_min_hours` / `ra_max_hours`); that's
  been replaced by the asymmetric CW-exclusion zone interval because
  the actual GEM hazard is one-sided.
  Defaults: CW exclusion zone `(0.95, 11.05) h` of mech_HA — the
  one-sided arc where the CW shaft rises more than 0.95 h above
  horizontal on the GTi (the symmetric `±6.99 h` natural-side
  bound from the 2026-05-13 test is *subsumed* by the new
  asymmetric model: tracking past `+0.95 h` enters the zone and is
  refused, while reaching `−12 h` for anti-meridian poses like
  Park 1 is no longer blocked). A narrower `(6.95, 11.05)` was
  used before 2026-05-17 but missed the ascending half of the
  CW exclusion arc — see
  [§Safety envelope](../../services/star-adventurer-gti.md#safety-envelope-cw-exclusion-zone)
  for the hardware session that triggered the widening. Altitude
  floor `0°` (geometric horizon) — see [§Altitude floor](../../services/star-adventurer-gti.md#altitude-floor).
- **Slew watcher abort on `:f` blocked** — both the slew and
  park completion watchers issue `:L` on both axes and clear
  `slew_in_progress` if either axis reports `blocked=true`.
- **Transient-error tolerance with best-effort halt** — the
  slew/park watchers tolerate up to three consecutive
  `poll_axes_now` failures (with a 100 ms backoff between
  attempts) before giving up, so a single USB-CDC glitch
  doesn't take the watcher offline for the rest of a goto.
  On retry exhaustion the helper fires `:L` on both axes
  best-effort so the motor isn't left commutating with no
  observer, then clears `slew_in_progress` and exits.
  Every successful poll is also emitted at `debug` with the
  full per-axis snapshot (position, running, blocked, goto)
  so post-mortems can reconstruct the last-known-good state
  observed before any failure. See
  `mount_device::watcher_poll_with_retry`.
- **EQMOD-style iterative pickup** — Phase A5 added a pickup
  loop in the slew watcher: after both axes report stopped,
  the watcher reads current RA/Dec, compares against the
  latched target, and if either residual exceeds 5″ (matching
  INDI's `RAGOTORESOLUTION`/`DEGOTORESOLUTION`) recomputes the
  delta for the current LST and re-enters the wire sequence.
  Capped at 5 iterations (`GOTO_ITERATIVE_LIMIT`). On the GTi
  this converges in 1–2 iterations because the post-stop
  residual is bounded by `slew_duration × sidereal_rate` and
  the second pass starts from a near-zero delta.

What's still outstanding from Phase 4:

- **Empirical slew rate vs `:g`** — the formal high-speed-ratio
  formula in spec §3 gives `1` for `hsr = 1` (sidereal rate),
  which is unusable. The firmware appears to pick its own
  Goto-mode rate (~5°/s observed). Documented; revisit if a
  tunable goto rate becomes a requirement.

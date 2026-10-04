# Star Adventurer GTi — MoveAxis: a rate override on one axis that hands the axis back

## Goal

`MoveAxis` is ASCOM's software hand-box: a client picks a rate from
`AxisRates`, the mount turns one mechanical axis at that rate until the
client sends rate `0`, and the axis then goes back to whatever it was
doing before. The [`star-adventurer-gti`](../services/star-adventurer-gti.md)
driver reports `CanMoveAxis = false` on every axis and an empty
`AxisRates`; the design doc defers the method as "manual hand-paddle-style
slew rates; not needed by `rp`". That is still true of `rp`. It is not
true of the clients an operator points at the mount directly:

- N.I.N.A.'s Three Point Polar Alignment plugin runs its **automatic**
  mode only through `MoveAxis` on the RA axis; with `CanMoveAxis = false`
  it offers manual mode alone, and the N/E/S/W buttons on N.I.N.A.'s
  telescope tab are greyed out ([plugin FAQ](https://github.com/isbeorn/nina.plugin.polaralignment/blob/master/PolarAlignment/FAQ.md)).
- SharpCap's polar-alignment slew buttons and SkySafari's direction pad
  drive `MoveAxis`, and SkySafari asks `CanMoveAxis` in its connect
  sweep. That is SkySafari pointed at this driver; through the
  [planetarium-bridge](../services/planetarium-bridge.md) it keeps
  reading the bridge's own `false`.

Issue [#1340](https://github.com/rusty-photon/rusty-photon/issues/1340)
holds the contract the implementation has to meet, as the ASCOM
specification states it and as Peter Simpson restated it on
[ConformU#32](https://github.com/ASCOMInitiative/ConformU/issues/32):

1. `MoveAxis` is a **temporary override**. It suspends the axis's
   current motion and replaces it with the given rate.
2. Rate `0.0` **restores the axis's previous motion** — sidereal
   tracking on RA if `Tracking` was on, otherwise rest.
3. `MoveAxis` **affects only the named axis**. A Dec override leaves RA
   tracking at whatever rate it had. ConformU does not verify this, so
   the BDD suite is the only guard.

The outcome of this plan: `CanMoveAxis` is `true` on the RA and Dec
axes, `AxisRates` advertises one measured range per axis, `MoveAxis`
honours the three rules above and the rest of the ASCOM contract
(`Slewing` reads `true` while an override runs, `Tracking` reads
unchanged across one, out-of-range rates are refused before any motion,
`AbortSlew` ends an override the way rate `0` does), an override is
refused where a slew would be refused — except that one moving away
from a limit is admitted as the operator's way back (D9) — and stopped
by a guard that keeps up with its rate, and a ConformU run on the field
rig records all of it.

## Why this is not "reuse the PulseGuide machinery"

The driver already runs rate-controlled motion: sidereal tracking and
every guide pulse are a `:G` (Tracking mode) + `:I` (step period) + `:J`
(start), ended by `:K`. Every wire primitive `MoveAxis` needs exists in
`crates/skywatcher-motor-protocol`. What does **not** exist, and why the
plan has the shape it has:

- **The fastest rate is unknown, and a stall is invisible.** The GTi
  answers the high-speed-ratio query with `1` on both axes, so the
  spec's formula for the fast regime gives sidereal, and the driver
  stopped relying on it (design doc, §Phase 4 findings). The only
  rate-controlled motion ever run on the hardware on purpose is
  Tracking-**Slow**, up to 8× sidereal (the #1299 probe's return leg).
  Tracking-**Fast** has run once, by accident: the Phase 4 codec bug
  that turned every goto into a continuous-step command. A goto was
  measured at about 5.1°/s, so the motors can go fast. But the GTi has
  no encoder: `:j` is the firmware's **commanded** step counter, a
  stalled motor keeps counting at exactly the commanded rate, and the
  one stall ever seen on the rig was heard, with the counter still
  advancing (design doc, §Phase 4 driver-logic changes). So the
  ceiling cannot be read off the wire at all; it is measured against
  an independent reference, by an operator. `AxisRates` is a promise to
  clients; it is made from that measurement, not from a formula.
- **An override has no deadline.** A pulse is a timed burst with a
  watcher that restores the axis; the ownership model
  (`pulse_guiding`, one id per axis under `axis_ownership`) is built
  around that watcher. An override runs until the client, a guard, an
  abort, a disconnect or a transport glitch ends it, so whoever takes
  an axis from an override has to stop it themselves — there is no
  watcher to defer to — and a glitch that would be harmless to a goto
  (it has a target) is unbounded motion for an override.
- **`Slewing` has a new source.** Today it is the goto reservation plus
  the `:f` goto bits. ConformU reads `Slewing` immediately after
  `MoveAxis(rate)` and expects `true`, and polls it to `false` after
  rate `0`.
- **The safety guard has to keep up.** The tracking-time guard watches
  `mech_HA` at sidereal speed with a 0.05 h margin. An RA override at
  3°/s covers that margin in a quarter of a second — less than one poll
  — and the mount coasts after a stop from speed (3.77° were measured
  after an instant stop from a 5.1°/s goto). The guard's margin, its
  stop ladder and its failure modes are all rate problems the tracking
  guard never had, which is why the guard ships **with** the driver,
  never after it.
- **The mock is wrong about the fast regime.** It seeds a high-speed
  ratio of 32 and gears Tracking-Fast by it; the hardware says 1. Any
  Fast-mode behaviour the driver relies on must come from the probe,
  and the mock must be re-seeded from the probe, or a green mock run
  proves nothing (see the project's own lesson on mocks written from
  the same spec as the code).

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| M0 | **Hardware probe.** `examples/probe_move_axis_rates.rs` measures, on the field rig with the service stopped and the operator at the pier: the rate ladder each axis follows in Slow mode against a physical reference, from the 24-bit floor to the first rung the operator sees it fail; whether Tracking-Fast differs from Slow at the same period; whether a live `:I` jump to a high rate is followed; stop latency and coast per rate for `:K` and `:L`; the `:i` readback after every `:I`; whether `:f` ever reports `blocked` on this firmware. Results go on the tracking issue, into the protocol notes, and into this plan's Decisions table | Not started | |
| M1 | **Design + BDD.** §MoveAxis lifecycle in the design doc (semantics, wire paths, refusals, take-over rules, the override guard, the ceiling constant), the capability, reads, methods, commands, configuration, module-structure and expected-report sections updated with their current-state cells marked planned (M2), `move_axis.feature` plus the pins in `pulse_guide.feature` and `auto_flip.feature`, all with step stubs under scenario-level `@wip`. Also closes #1340 item 2: the North/South PulseGuide scenarios pin "no RA frame" and `Tracking` unchanged, and the per-axis sentence lands in §PulseGuide lifecycle | Not started | |
| M2 | **Driver and guard, one deliverable.** Per-axis ownership generalised from "pulse or nothing" to "pulse, override, or nothing"; `CanMoveAxis` / `AxisRates` / `MoveAxis`; the restore ladder; `Slewing`; the refusals and take-overs in slew, sync, park, abort, `Tracking`, `PulseGuide`, disconnect and reconnect; the override tick in the tracking-guard task with its rate-aware margin; mock re-seeded from M0; `@wip` removed, suite green; in-tree ConformU on the mock clean. No new config key; `CanMoveAxis` flips to `true` only in the commit that carries the guard | Not started | |
| M3 | **Hardware validation.** Full two-suite ConformU on the field rig, filed as a `docs/validation/` record with its index row; then the motivating clients against the driver: N.I.N.A.'s telescope-tab paddle and TPPA automatic mode | Not started | |
| M4 | **Fast regime** — only if M0 shows Tracking-Fast reaches rates Slow cannot and clients want them. Otherwise the plan ends at M3 with a Slow-only rate range | Conditional on M0 | |

M0 gates everything after it: the `AxisRates` numbers, the live-change
rule, the guard margin's coast term, the override stop-and-wait budget
and the mock's fast-mode gearing all come from it. M1 can be drafted
alongside M0 and is finalised once the numbers are in. M1's #1340
item-2 pins are independent of all of this and may land first as their
own small PR. M2 is one phase because an override without its guard is
not a shippable state; if it has to split across PRs, the first PR
keeps `CanMoveAxis` at `false` and the MoveAxis ConformU groups off, and
the capability flip, the constant ceiling and the guard land together,
so an unguarded override is never a shippable state (decided
2026-10-03, over a driver-only PR behind a build-time switch).

## Decisions

Every row was decided with the operator on 2026-10-03, on the recommended option; the text keeps the alternatives considered and why they lost, so the reasoning survives the decision.

| # | Question | Recommendation | Status |
|---|----------|----------------|--------|
| D1 | Sign of the rate | **Mechanical on both axes, with the Dec sign fixed per hemisphere** — the ASCOM simulator's rule, which rp's BDD already runs against. **Primary**: positive turns the RA motor CW, the direction sidereal tracking turns, so the OTA moves west (`mech_HA` increases). **Secondary**: positive moves the OTA toward +Dec in the normal (counterweight-down) pointing state at the configured site latitude, and that motor direction is kept through the pole, so `−r` always retraces `+r` (ConformU's "move back" legs, and every paddle); with a southern site latitude the Secondary sense is inverted once, exactly as OmniSim does. There is **no** pointing-state inversion: a sign resolved against the pier side would stop retracing the moment a leg crosses the pole, which ConformU's legs can do from where they start (near the pole, where the AbortSlew test leaves the mount). The Alpaca API says only "the rate of motion (deg/sec) about the specified axis"; the ASCOM definition leaves the sign "purposely undefined" and calls the axes mechanical. The pulse's celestial inversion is a guiding contract, and a paddle maps buttons to signs itself (TPPA measures the direction it gets, N.I.N.A. has reverse toggles). The design doc records the client-visible consequence: past the pole, positive Secondary moves the OTA toward −Dec | Decided 2026-10-03 |
| D2 | Shape of `AxisRates` | One contiguous range per axis, `[min, max]` in deg/s, both axes advertised; Tertiary stays empty with `CanMoveAxis = false`. The pair is **one `f64` pair computed once** per axis; `AxisRates` returns it, `MoveAxis` validates `\|rate\|` inclusively against the same pair, and the rounded period is clamped to 24 bits after rounding in the `MoveAxis` caller only, so the exact values ConformU reads back are the exact values it may send. `min` is a per-axis constant M0 decides: the 24-bit `:I` floor (period `0xFFFFFF`, ≈ 0.023 × sidereal on RA, ≈ 0.028 × on Dec, about one step per second) times a floor multiplier that is `1.0` if the floor rung runs and stops cleanly — its measured motion matches the command over a hold long enough to resolve it — and the lowest clean rung's factor otherwise. The refusal of `min / 2` comes from the inclusive range check against the advertised pair; when `min` is the floor it is also unexpressible, since no 24-bit period exists below it. `max` is the D3 ceiling constant — M0's firmware ceiling for the mount code times its safety factor — clamped at connect by the D9 margin cap, so the pair is computed once from the connect-time cap and never from a config key. One range cannot overlap or duplicate, which is what ConformU checks, and a client gets every rate in between rather than a preset ladder | Decided 2026-10-03 |
| D3 | Rate limits live where | **Nowhere new.** `MoveAxis` is enabled on RA and Dec whenever the connect-time range is non-empty, which every realistic config gives, and the ceiling is a driver constant: M0's firmware ceiling for the GTi mount code (which the identity gate already reads at connect) times a safety factor set from the record, measured on a loaded rig. At connect the constant is clamped by the guard-margin cap (D9) computed from the existing `command_timeout` and `polling_interval` keys, so a rig with a long timeout gets a lower ceiling without a new key — about 4.3°/s on a default config (2 s timeout, 200 ms poll) with the 3.77° coast prior — and `AxisRates` reports the clamped value. The two keys are unconstrained `Duration`s, so a config can push the cap below an axis's `min` — it takes a `command_timeout` or `polling_interval` of tens of thousands of seconds — and the two minimums differ (D2), so the cap can fall between them. The range is computed **per axis**, and an axis whose range is empty is disabled on its own: `CanMoveAxis` reads `false` for it, its `AxisRates` is empty, `MoveAxis` on it is `NOT_IMPLEMENTED` before the parked and rate checks, the other axis is untouched, and connect `warn!`s naming the axis, the keys and the cap. The feature degrades rather than the config failing; a config that loaded before this plan still loads. Nothing on the wire can derive the ceiling (the high-speed ratio reads 1, there is no encoder, a stall is invisible), so a typed-in per-rig ceiling would be one the operator cannot verify; a `move_axis` config block (a lower ceiling for an unusual payload, or a switch to refuse paddle clients on an unattended rig) is added only if M3's rig run shows the need | Decided 2026-10-03 |
| D4 | A new rate on an axis already overridden | Replace it. `OverrideId` is per call. A live `:I` is used only for the shapes M0 measured (same direction, same regime, within the jump M0 showed the motor follows) **and** only after a live `:f` read under `axis_ownership` shows the axis running in the claim's mode — the pulse's `admits_live_rate` gate, never the claim's recorded mode alone, because a claim whose start is still in its stop-and-wait describes a stopped motor. Anything else → stop-and-wait, `:G`, `:I`, `:J`. Every change, live or not, re-runs the D9 start gate at the **new** rate under the lock before its first frame — a higher rate widens the margin, so a pose that admits `r` may refuse `2r` — and a refused change sends nothing and leaves the axis at its current rate. A call whose start or change is superseded by a later `MoveAxis` on the same axis returns `INVALID_OPERATION` ("superseded") and touches the wire no further; the later call owns the axis. The connection's `live_rate_refused` latch applies: once the mount has refused a live change, every change stops first | Decided 2026-10-03 |
| D5 | Rate `0` | On RA with `Tracking = true`: if a live `:f1` shows RA running in Tracking/Slow/CW and the latch is clear → a live `:I1 <sidereal>`, exactly the pulse restore; otherwise → `:K1`, wait stopped, `:G110`, `:I1 <sidereal>`, `:J1`, the `Tracking = true` sequence, as the pulse's `Restore::Restart` does. With `Tracking = false`, or on Dec → `:K`, wait stopped. The ambiguous-reply retry and the `:K` → `:L` stop ladder are the pulse's, reused; a restore that cannot be made to land stops the axis and, on RA, sets `Tracking = false`, as the pulse does. The claim is released only after the restore lands — **never on an unconfirmed stop**: a stop the budget cannot confirm, on any path (rate `0`, `AbortSlew`, a start's or change's stop-and-wait, Park's take-over), leaves the claim held in a stopping state, the call returns an error naming it, `Slewing` stays `true`, and the override tick (D9) carries the `:K` → `:L` retries until `:f` confirms, then clears the claim, sets `Tracking = false` on RA because the restore never landed, and `warn!`s. The pulse's stop helper is reused for its wire sequence only; the pulse's own release after an unconfirmed ladder is not a shape the override copies. So `Slewing` reads `true` through the restore and no pulse or slew can claim the axis during the stop-and-wait. Rate `0` on an axis with no override is a no-op success — ConformU's first call — but only **after** the connected, implemented and parked checks: ConformU sends rate `0` to a parked mount and requires `INVALID_WHILE_PARKED`. The override's stop-and-wait budget is a named constant set from M0's stop latency at the ceiling, not the slew's 2 s. Rate `0` is synchronous: it returns once the stop is confirmed and the restore has landed, because a paddle client's next read is `Slewing`. ConformU times only the first rate `0` (no override) and the `+min` / `+max` starts from rest or sidereal, never a stop from speed, so its 1 s target does not constrain the shape | Decided 2026-10-03 |
| D6 | `Tracking` written while an override runs | **Refused** with `INVALID_OPERATION` ("MoveAxis in progress; send rate 0 first") while any override claim is held, on either axis. Deferring the write instead was considered and rejected on two counts: the tracking-guard and auto-flip ticks key on `tracking_requested`, so a value that changes mid-override hands RA to two guards with different margins; and a `Tracking = true` write holds `axis_ownership` through a 2 s stop-and-wait, which at 5°/s is 10° the override guard cannot interrupt. The spec's own FAQ says clients should not rely on the `Tracking` value while `MoveAxis` is in effect and lists no exception for the write, so any policy is within it; OmniSim accepts the write and applies it when the move ends; ConformU writes `Tracking` only between overrides, after `Slewing` has read false, so the refusal is never reached by its sequence. The error names the way out | Decided 2026-10-03 |
| D7 | Slew, sync, `SetSideOfPier`, `Park`, `AbortSlew` while an override runs | **Slew, sync and `SetSideOfPier` refuse** with `INVALID_OPERATION` ("MoveAxis in progress; send rate 0 first"): `Slewing` already reads `true`, so a client that slews anyway is confused; the snapshot a sync classifies from is a poll interval stale at degrees per second; and the override's bursts need the lock a sync holds across its writes. The refusal lives **inside** the `axis_ownership` hold that acquires the slew reservation, before `try_acquire`, and `start_override` checks `slew_in_progress` under the same lock, as `begin_pulse` does — a check outside the lock is a take-over, not a refusal. `SetSideOfPier` inherits it through the slew path; sync's goes next to its existing flag check under the lock it already holds. **`AbortSlew` ends every override exactly as rate `0` would**, per axis, so RA goes back to sidereal when `Tracking` was true and `Tracking` reads unchanged — ITelescope V4 requires it ("In the case of MoveAxis() … Tracking must be returned to the state before the slew stopped"); `Slewing` reads `true` until the restores land, and an unconfirmed stop keeps the claim with the tick, per D5. The goto-abort behaviour (`:L1` `:L2`, `Tracking` off) is unchanged and the two cases are disjoint, because no override can exist during a goto. With **mixed claims** — an override on one axis and a pulse on the other, which D8 allows — each axis ends by its own claim's abort rule: the override axis as rate `0` would, the pulse axis as today's abort ends a pulse (cancelled, `:L`, and `Tracking` off when that axis is RA, because today's abort clears `tracking_requested` for any pulse it cancels). `Tracking` is RA's state and follows RA's claim — unchanged after an RA override, off after an RA pulse — so neither contract moves. **`Park` takes over** — not because it is stop-class (it is stop-then-goto to the park ticks, with no envelope check) but because safety automation sends it and must not be refused. It stops the claimed axes and waits — with the override stop-and-wait budget from D5 on an axis that held an override claim, not the slew's 2 s — as it already does for pulses; a stop that budget cannot confirm refuses the park with `INVALID_OPERATION` and leaves the claim with the tick, per D5. Park runs an RA path check before its goto, **unconditionally** — not behind a flag that remembers an override, because the pose outlives the process and no in-memory provenance does: the firmware counter is preserved across a restart and a `config.apply` rebuild (design doc §Safety stop at startup), so a crash or reload during or after an override leaves a pose the next process has no record of. The check is on the sweep Park actually commands (the raw tick delta from the current count to the park target, in the sense of the direction bit it sends), not the slew's folded canonical delta, because after an override that folded past ±12 h the two differ. The check refuses a sweep that passes **through** the zone — enters it and leaves it — on its way to a target beyond it; a sweep that ends inside the zone at the configured target is the destination's privilege and is unchanged, because the design doc allows a park target inside the zone (park writes ticks without the zone check) and from any pose the sweep to such a target must enter it. On a pose a slew, tracking or a guard stop reached the check passes — none of those leaves the mount beyond the zone, so every park that works today still does — and a sweep through the zone is refused with `INVALID_OPERATION`; the mount then stays stopped with `Tracking` off and the operator backs out with `MoveAxis`. An override is the first primitive that can leave the mount at a pose no slew could have reached, so the privileged park keeps its exemption for the destination only, never the path. **`SetPark` refuses too**, while any override claim is held — its captured pair must be one pose — and takes its capture under `axis_ownership`, so an override cannot start between its wire read and its write; today it refuses on `slew_in_progress` and a fresh wire `running` read, neither of which sees a claim | Decided 2026-10-03 |
| D8 | `MoveAxis` on an axis with a pulse in flight, and the reverse | `MoveAxis` takes the axis from the pulse, as a slew does: the pulse's id is cleared under `axis_ownership`, its watcher sends nothing, and **from that moment the override owns the axis** — a start that fails afterwards ends the axis as the pulse's `end_failed_start` does (sidereal back on RA with `Tracking` on, else `:K` / `:L`). `PulseGuide` on an overridden axis refuses with `INVALID_OPERATION`, naming the override. `PulseGuide` on the **other** axis is allowed: rule 3 cuts both ways, so `pulse_guide`'s pre-check moves from `slewing()` (which the override now makes true) to the goto reservation and `:f` goto bits, and the per-axis refusal comes from the claim. `IsPulseGuiding` counts pulse claims only, never overrides; `Slewing` counts override claims only, never pulses (ASCOM: "Slewing must not be True during PulseGuide() operations"). The spec says `MoveAxis` is not for guiding; a guider pulsing while an operator paddles is already in trouble, but it is not the driver's job to make the free axis unusable | Decided 2026-10-03 |
| D9 | The guards | **Start gate, before any frame** (the slew's envelope check, in kind), evaluated **twice**: on the poll snapshot before the lock, to refuse without a wire frame, and again under `axis_ownership` on a fresh `:j` read immediately before the **motion burst** (`:G` `:I` `:J`) — on the paths that stop first, after the stop-and-wait has reacquired the lock, since the `:K` before it is stop-class and needs no gate — so a sync that lands before the lock or during the stop-and-wait (the window #1311 describes) cannot start an override from a verdict its new frame invalidated: an RA override whose direction leads toward the exclusion band is refused with `INVALID_OPERATION` when the snapshot `mech_HA` is already inside the rate-aware band; **any** Dec override is refused while `mech_HA` is inside the hard zone — the hazard the zone exists for is the OTA sweeping into the pier on a Dec rotation at counterweight-up, which the altitude floor does not see; and an override on **either** axis whose direction lowers altitude is refused when the lowest altitude along its projected travel (the tick's floor rule, below) is at or below `min_altitude_degrees` — the same shape as the band gate, so a start from just above the floor cannot cross it before the first tick. A direction *lowers* when the lowest altitude along its projected travel is below the current altitude, never by the sign of the instantaneous slope: for RA (hour angle enters the design doc's altitude formula through `cos(HA)`) that is `+Primary` west of the meridian and `−Primary` east of it, and on the meridian itself both, since the slope's sign vanishes there while both travels descend. The direction is the overridden axis's own, because that is the motion the stop controls; the other axis's motion under rule 3 is tracking's or a pulse's and is not the override's to stop. With `Tracking = true` during a Dec override, RA's sidereal motion carries the pointed sky setting through the floor exactly as it does with no override: today the floor gates slew and sync targets and the tracking-time guard watches the band only (design doc §Tracking-time safety guard), so tracking through the floor is a pre-existing exposure this plan neither widens nor closes — a raising Dec override only adds altitude to it, a lowering one is stopped at a margin that includes the sidereal term, and a tracking-time floor guard is its own follow-up, listed under out of scope. An override **away** from the band or the floor is always allowed: that is how an operator backs out after a guard stop. "Toward" and "away" are defined on the folded `mech_HA` circle: outside the zone, toward means the direction whose next zone edge is nearer than ±12 h of travel from the current count's side; inside the zone (a coast or a failed stop can leave the axis there), away is the direction back to the edge the axis entered from, recorded at the guard stop, and the nearer edge when no entry was recorded. **Tick, the backstop**: the tracking-guard task gains an override tick, run first in `guard_loop_tick`, that reads the **projected** sample while a claim is held and stops an override moving **toward** a limit once the projected sample is within the margin of it or past it — RA toward the band, either axis toward the floor — and never one moving away: the raising direction below the floor and the direction out of the zone are the recovery the start gate admits, and they run until rate 0. It also stops a **Dec** override whenever the projected `mech_HA` is inside the hard zone, whatever carried it there — with tracking off, a perpendicular RA pulse (D8) can, from a start within one pulse's travel of the edge — because the start gate's refusal of a Dec override inside the zone has to hold for the override's whole life, not only at its start; the pulse itself is left to its own watcher. The margin is a stated latency budget, not a poll count: `margin = rate × ((S + 1) × polling_interval + command_timeout) + coast(rate)`, where `S` is the override's **own** staleness cap in polls, a constant of `2` — tighter than the manager's four-poll projection cap, which is sized for reads, not stops, and would let a frozen poll stay actionable for five intervals — so a frozen poll becomes a `:K` by the tick after the sample turns `S` polls old and the budget covers it; in hours of `mech_HA` for the band, with `coast(rate)` from M0 (prior: 3.77° from 5.1°/s). For the floor the margin is not a rate times a time: the guard projects the pose forward by that same mechanical travel — the overridden axis at its motor rate over the latency budget plus its coast, the other axis by its commanded travel under whatever claim it holds (sidereal when tracking, the pulse's rate shift while a perpendicular pulse is in flight) — and compares the **lowest altitude along that projected travel** with the floor, because the instantaneous altitude rate is zero on the meridian while both RA directions descend there, and a margin built from it would admit a start just above the floor that the latency alone carries below it. D3 rejects a ceiling whose margin exceeds the 1.0 h cap. While an RA override claim is held the tracking tick skips RA; while **any** override claim is held the auto-flip tick is inert and its once-per-crossing latch untouched (its flip is a slew, which D7 refuses under any override claim, and today a refused attempt spends the latch), so a paddle on either axis across the offset neither double-stops RA nor spends the crossing's flip. **A guard stop is confirmed, not acked**: the tick sends `:K`, keeps the claim, and on later ticks polls `:f` until `running` clears, escalating to `:L` when it does not (the slew watcher's shape, not the pulse's 3 × 50 ms ladder); only a confirmed stop clears the claim, sets `Tracking = false` if it was RA, and `warn!`s. A `:K` or `:L` that fails or is ambiguous is retried on the next tick; while nothing has confirmed, the claim stays, `Slewing` stays `true`, the override tick keeps watching, and abort or disconnect remain the operator's exits — an acked stop on a mount still decelerating must never leave the axis unwatched. The guard never restores tracking into the band. A snapshot older than `S` polls while a claim is held is itself a stop condition — the override's own cap above, not the manager's — so a stalled poll loop cannot leave the guard reading a pose the axis left. `blocked` from `:f` is a stop condition too, and it takes the emergency path, not the ladder: the slew and park watchers already answer `blocked` on either axis with an immediate `:L` on **both** axes, and the override tick does the same — `:L1` `:L2` at once, no `:K`, every claim on either axis cancelled as `AbortSlew` cancels a pulse, the claims cleared once `:f` confirms the stop, `Tracking = false`, and a `warn!` naming the blocked axis — because a blocked gearbox must not strain through a deceleration and the other axis must not keep moving. It is defence in depth only: no run or probe to date has seen the GTi set it, and M1 records that in the design doc with its evidence, M0's experiment 6 included. With `cw_exclusion_zone` at `null` only the zone checks are off — the band gate, the Dec-in-zone rule and the band stop — as the tracking guard's are today; the override tick itself keeps running, for the floor, the staleness cap, the transport-failure latch and `blocked`. The config note says plainly that a negative `min_altitude_degrees` widens the paddle's mechanical reach | Decided 2026-10-03 |
| D10 | Disconnect, reconnect, service stop | `Connected = false` stops every overridden axis, as it stops every pulsed axis, before the session goes. A crash sends nothing, so the motor runs at the override rate until the next start, where the design doc's unconditional start-time halt (`:L1` `:L2` `:K1`, stop-class under tenet 3) stops it; that exposure is the restart's latency at the ceiling plus the coast, and this plan adds nothing to that path. The last-disconnect safety stop (`:L1` `:L2` `:K1`) already covers a client that vanished. **A transport glitch under a live client** keeps the design doc's reconnect contract — the reconnect itself halts nothing, and no device-side hook runs on it (the shared transport's hooks are built before the device exists and cannot see its claims). The override ends through the guard instead, and the trigger is the failure itself, not only its duration: a transport failure observed while an override claim is held — a poll or a send that failed at the transport (a timeout, a closed port), never one the firmware answered with `!` or that failed to decode, before any reconnect — latches a stop on the claim, which the override tick executes on the first tick after recovery; while the link is down the `:K` fails each tick and the claim is kept per D9, and a glitch shorter than the staleness cap is ended by the latch where the staleness rule alone would not have fired. An override is a client-held motion with no deadline, so a link known to have dropped under it cannot vouch for the client still holding the button. The confirmed stop clears the claim, sets `Tracking = false` if RA was claimed, and `warn!`s. The client's `Slewing` then reads `false` and its next `MoveAxis` starts afresh. A goto survives a glitch because it has a target; an override has none, and the firmware has no motion deadline of its own (`:J` runs until a `:K` reaches it), so **there is no bound while the link is down**: the design doc records the exposure as the outage's duration plus the supervisor's recovery latency plus one tick, at the ceiling, plus the coast, and names the mount's power switch as the only exit during the outage. That is the one risk no ceiling bounds, and the unattended-rig switch D3 keeps in reserve exists for it. Every one of these is stop-class, and nothing on any connect, handshake, poll or `config.apply` path starts an override: tenet 3 holds | Decided 2026-10-03 |

## Phase M0 — the probe

An operator-run bench tool in the shape of
`examples/probe_live_step_period.rs`: service stopped, operator at the
pier with a hand on the power switch. It is the instrument the design
needs, not an implementation of it, and the development workflow's
"no code before scenarios" rule is about the driver; the #1299 probe
set the precedent.

**Preconditions, asserted before the first motion frame.** `:e1`
identity, the CPRs and `TMR_Freq` the design doc records, **`:g1` /
`:g2` both reading `1`** (the Fast-mode experiment and the mock
re-seed both rest on it), both axes stopped, and the mount carrying
the heaviest imaging train it will be given, at least the field rig's,
with the mass in the record: the ceiling is a mount-wide constant (D3)
and a stall is load-dependent, so a ladder run on a bare mount would
license a rate a loaded one cannot follow. The frame must be
anchored: the operator asserts the physical pose by eye (Park 3 —
counterweights down, OTA at the pole), and the probe seeds the counter
to that pose's tick pair with `:E` before anything else, as the
driver's own named-park seed does, rather than trusting whatever the
firmware's counter reads after a power cycle. Every travel bound below
is computed from that asserted pose, and the probe refuses to start
if a full ladder's travel plus the measured coast would touch the
exclusion band — the CW ladder against the band's lower edge, the CCW
ladder against its upper edge through the ±12 h wrap — or take Dec
outside a tick bound about the start pose that keeps the OTA above the
horizon at the rig's latitude. Every exit path — a finished run, an
error, `SIGINT`, `SIGTERM`, `SIGHUP` and `SIGQUIT` (the #1299 probe handles all four, so a dropped SSH session cannot leave a rung running) — stops both axes, waits for `:f` to
confirm, and escalates to `:L`. A power cut is the operator's exit of
last resort; it zeroes the firmware's counter, so the record says that
after one the mount is physically re-parked before the service starts.
A rung that fails its ratio or stalls un-anchors the frame the same
way — `:j` kept counting while the mechanics lagged, so the equal
commanded return cannot be trusted to find the mark — and the probe
refuses any further trial until the operator has physically re-parked
to the mark and it has re-seeded `:E`; the record notes each such
re-anchor.

**What the tool can and cannot see.** `:j` is the commanded step
counter: the firmware reports the rate it is stepping at, whether or
not the motor follows. Every rung therefore carries an independent
motion reference, and it measures the **outbound leg**, not only the
return: a round trip that comes back to its mark proves nothing about
rate, since a motor following both legs at the same fraction of the
command returns too. The reference is a camera on the OTA for the
rungs whose hold keeps its target in the frame, and an angular scale
fixed to the axis, read by the operator, for the rungs that travel
degrees. At the asserted Park 3 pose the OTA lies along the polar
axis, so an RA rung rotates the field about the optical axis instead
of translating it: the camera's RA reference is the field's rotation
angle, fitted from two or more stars or features, which equals the
mechanical RA angle with no plate-scale term; the Dec reference is
pixel displacement over the hold at the known plate scale;
each hold is 2 s or as long as it takes for the commanded displacement or rotation
to reach ten times the reference's resolution, whichever is longer (at
the floor, one step a second, that is a minute under the camera). The
record carries the ratio of measured to commanded motion per
rung, and a rung passes only at a ratio of 1 within the reference's
resolution. Each rung still ends with an equal CCW leg back to the
physical index mark (the pose the operator asserted): return-to-mark
and the absence of stall noise are the stall-asymmetry check, and both
are in `summary.json` per rung. The record calls the fitted count rate
the **firmware-reported step rate**, never the measured rate, and the
ladder stops on a ratio below 1, operator-observed non-motion, audible
stall, or `running` dropping — not on a count that cannot disagree
with the command. Samples are stamped at the
send instant, which #1371 established is when the firmware latches
the count.

Experiments, per axis:

1. **Slow-mode rate ladder.** From rest: `:G` Tracking/Slow, `:I`, `:J`;
   hold 2 s, or as long as the reference needs (above); `:K`; the equal
   return leg. Rungs: the floor `0xFFFFFF`,
   three sub-sidereal rungs, then 8, 16, 32, 64, 128, 256, 512, 800,
   1200 × sidereal; if the top rung passes, the top rung is the
   ceiling candidate — the probe never climbs above the goto rate,
   because no paddle needs to. The coarse ladder finds the candidate;
   it does not validate the interval, since a stepper can lose steps
   in a resonance band between two passing rungs. So a **fine ladder**
   follows, four rungs per octave (ratio ≈ 1.19) from the floor to the
   candidate, each held and measured like a coarse rung, and the
   advertised `max` is the highest fine rung below the first fine rung
   that fails, times the safety factor; a band found inside the
   interval truncates the range below it rather than punching a hole
   in it, since D2 advertises one contiguous range. A band narrower
   than one fine step can still hide between rungs; the record says
   so, and the safety factor is the allowance for it. After every `:I`
   and every `:G`, an `:i` readback:
   a firmware that clamps a too-small period (INDI's `minperiods`) is
   seen directly, and a clamp is the likeliest shape of "the top of the
   ladder is the goto rate". Record the step rate fitted over hundreds
   of milliseconds (the count is bursty at speed), the `:f` bits
   including `blocked`, whether the rate ramps or jumps after `:J`, and
   the operator's observation.
2. **Fast mode.** `:G` Tracking/Fast at the same periods, starting from
   the **lowest** rung with its own budget: the hold is cut the moment
   the reported rate exceeds the Slow ceiling from (1) or a hard cap
   (3°/s), because a hidden gearing of 32 would turn the 128× rung into
   34° in two seconds, and the ladder stops at the first cut rung. A
   cut hold returns to the mark in Slow mode at 8×, never at the Fast
   period. The preflight budgets each Fast rung at its **worst case**,
   not at the cap, because the cap bounds what the cut rule reacts to
   and not what the motor does before it reacts: the rung's rate times
   `G_max`, a probe parameter defaulting to 32 (the hidden gearing the
   cut rule's own example assumes), for the fit window plus the
   Slow-mode stop latency, plus the coast at that worst-case rate —
   taken from experiment 4, which runs on the Slow ladder before this
   one, scaled linearly above the Slow ceiling and never below the
   3.77° prior. A rung whose worst-case travel would leave the bound is
   not run and the ladder ends there, with the fact in the record;
   starting at the floor keeps the first rungs inside any bound, since
   the floor at `G_max` is still sub-sidereal. A gearing above `G_max`
   is outside what the probe can bound, and the record says so.
   Outcomes: identical to Slow (the ratio really is
   1), geared (the gearing is measured, and M4 exists), or a different
   ceiling. INDI eqmod drives this family in Fast at its 600–800×
   presets with `period = sidereal / rate`, which is the prior art for
   expecting linearity.
3. **Live `:I` to a high rate.** With the axis running Slow at sidereal,
   a single live `:I` to 64×, 256× and the Slow ceiling from (1), then
   back; on RA both CW and CCW, and on a running Dec. Also the floor,
   live from sidereal. Does the motor follow, with or without a ramp?
   This decides D4's live-change rule and the largest jump it allows.
   If M4 is taken, one Fast-mode live `:I` trial, or D4 decides that
   Fast overrides always stop first, as INDI does.
4. **Stop latency and coast per rate**, taken as each Slow rung's stop
   in (1), so it exists before (2) runs. `:K` from each rung: time to
   `running = 0`, ticks of coast; the same for `:L`. The coast at the
   ceiling is D9's `coast(rate)` and the probe's own travel margin; the
   latency is D5's stop-and-wait budget, which Park inherits when it
   takes an override over.
5. **Rate change while running, same direction.** `:I` between two
   high rungs on a running Slow-mode axis, up and down.
6. **Does `:f` ever report `blocked` on firmware 3.48?** Recorded as a
   yes or no from every trial, including any stall the operator
   provokes on purpose at the top of the ladder with a hand on the
   switch.

Outputs: `exchanges.jsonl` and `summary.json` as the existing probe
writes them, attached to the tracking issue with the tables (the probe
is not a ConformU run, so there is **no** `docs/validation/` record —
the 2026-09-28 entry in the design doc's §Real-hardware validation is
the shape: a dated entry that says so); the measured facts into
`docs/references/skywatcher-motor-controller-command-set.md` under
"Empirically verified"; and the decision inputs into this plan's
Decisions table with the date.

**Safety of the probe itself.** Each hold is a bounded sample loop
followed by `:K`; the abort flag is checked at every sample and ends a
hold early; every exit runs the safety stop. There is no independent
watchdog task — the tool holds the transport alone and runs
sequentially — so the operator's hand on the switch is the last
resort, and the record says so. The probe never issues a goto and
never touches the axis it is not measuring. Tracking-Fast is the mode
of the Phase 4 runaway; experiment 2 runs it under the cut rule above,
from the lowest rung, each rung budgeted at its `G_max` worst case.

## Phase M1 — design and scenarios

### Design doc additions (`docs/services/star-adventurer-gti.md`)

The design doc's capability, methods, commands, configuration and
expected-report sections are current-state contracts. Every M1
addition to them lands marked **planned (M2)** in the cell or section
lead — the way `@wip` marks the scenarios — and M2 removes the markers
in the commit that flips `CanMoveAxis`, so a reader between the phases
sees the current contract (`CanMoveAxis = false`, `MoveAxis` →
`NOT_IMPLEMENTED`) and the planned one side by side, never the planned
one as current. §MoveAxis lifecycle is design and carries no marker.

- **Capability flags**: `CanMoveAxis(Primary)` / `(Secondary)` → `true`
  per axis, whenever that axis's connect-time range is non-empty (D3); `(Tertiary)` →
  `false`. `AxisRates` per D2.
- **Writes / methods**: `MoveAxis(axis, rate)` row with the refusals in
  order: `NOT_CONNECTED`; `NOT_IMPLEMENTED` for Tertiary or for an
  axis whose connect-time range is empty (D3); `INVALID_WHILE_PARKED`; then the rate-`0` no-op; then
  `INVALID_VALUE` for a non-finite rate or `|rate|` outside the axis's
  range; `INVALID_OPERATION` while `slew_in_progress` is set (a slew or
  park, including its dwell, pickup and settle — checked under
  `axis_ownership` as `begin_pulse` does, so no override claim can
  exist when `finish_slew` or the pickup loop runs and those paths stay
  claim-blind) and for the D9 start gate. Returns once the rate is on
  the wire.
- **§MoveAxis lifecycle**, in the register of §PulseGuide lifecycle: the
  wire diagram for start (from rest, from tracking via live `:I`, from
  an existing override via D4), the rate `0` restore per D5, the
  refusals and take-overs per D6–D8 with the lock discipline (burst
  under `axis_ownership`, never across a stop-and-wait, re-check the
  claim and the start gate), the guards per D9, disconnect and reconnect per D10, the
  mechanical sign convention per D1 with its client-visible
  consequence past the pole, the stall blind spot (a stalled override
  corrupts the pointing frame because the count says the axis moved;
  a suspected stall needs a re-sync; the mount-wide ceiling constant
  with its safety factor is the only mitigation, and the loads it was
  measured and validated on are named), the statement that no run or
  probe has seen `:f` report
  `blocked` on this firmware, with the runs and M0's experiment 6 as
  its evidence, and the step-period arithmetic: factor
  `|rate| / SIDEREAL_DEG_PER_SEC`, period `round(sidereal_period(axis) /
  factor)` clamped to 24 bits, using the addressed axis's CPR — the same
  per-axis rule the pulse learned the hard way.
- **Reads**: `Slewing` gains "or an override claim is held on either
  axis" (pulses excluded). `Tracking` is unchanged by an override and
  refuses writes during one (D6). `IsPulseGuiding` ignores overrides.
- **Configuration**: no new key. The section documents the constant
  ceiling and its safety factor, the margin-cap clamp at connect from
  `command_timeout` and `polling_interval` with the ceiling that leaves
  on a default config, and the note that a negative
  `min_altitude_degrees` widens the paddle's mechanical reach.
- **Commands used by the MVP**: `:G` Tracking/Slow in both directions
  (and Fast if M4), `:I`, `:J`, `:K`, `:L` gain the override rows.
- **Module structure**: the second example (M0), `mount_device/override.rs`,
  the renamed step-period helper, `move_axis.feature` and
  `move_axis_steps.rs`.
- **Deferred table**: the `MoveAxis` row is removed; a Tertiary axis and
  rates beyond the Slow ceiling (if M4 is not taken) are listed instead.
- **Expected ConformU report** and **Running ConformU manually**: the
  MoveAxis Primary / Secondary groups now run; the expectation stays
  0 / 0 / 0 against the in-tree mock config. What ConformU does, so the
  log is predictable: per axis, rate `0` with no override; `min / 2` and
  `max + 1` refused; `+min`, `0`, `−min`, `0`; `+max`, `0`, `−max`, `0`;
  then the retained-state check, `+max`, `0`, `Tracking` toggled,
  `−max`, `0` — four 2 s legs at the ceiling, two each way, plus two at
  the floor. The mount is wherever the AbortSlew test left it: the park
  pose plus about 1.5 s of an aborted goto toward HA −3, with `Tracking`
  off, so MoveAxis Primary enters the retained-state check with
  `Tracking` false and MoveAxis Secondary with it true, and both
  branches run. The mock config sets `unpark_from_ap_position` to
  `ap_park_3` (`mech_HA` −6 h) so a 2 s leg at the ceiling plus its
  margin stays clear of the band; with the ship default the mock starts
  at `mech_HA` 0, 0.9 h from the band edge, which a ceiling leg plus
  the rate-aware margin would breach.
- **§PulseGuide lifecycle** gains the sentence #1340 asks for: a pulse
  on one axis sends nothing to the other, and an RA restore returns
  the requested state.
- **§Safety stop across a reconnect** gains a paragraph on overrides:
  the contract is unchanged (a reconnect under a live client halts
  nothing), an override in flight across a glitch ends through the
  guard tick per D10, and the bound that leaves.
- Documents that state what is **implemented** change in M2, not here:
  `docs/workspace.md`'s service row (the service has no README of its
  own) and `device_metadata.feature`'s description. The ConformU config
  M2 edits is the inline `serde_json::json!` in
  `tests/conformu_integration.rs` (the checked-in
  `conformu-test-config.json` is referenced only by an archived plan);
  `pkg/doctor.toml` carries the port and USB identity only and needs
  nothing.

### `tests/features/move_axis.feature`

Scenario titles state outcomes; the wire frames and the rate numbers are
in the feature file, computed for the mock's CPRs and `TMR_Freq`. The
list, grouped as the design doc sections are:

- **Capabilities.** CanMoveAxis Primary and Secondary read true;
  Tertiary reads false. AxisRates for each axis is the pinned
  `[min, max]`; Tertiary is empty. With a `command_timeout` long enough
  for the margin cap to bite, AxisRates reports the clamped ceiling;
  with one long enough to push the cap below both minimums, CanMoveAxis
  reads false on both axes, AxisRates is empty and MoveAxis is
  `NOT_IMPLEMENTED`; with one that lands the cap between the two
  minimums, the axis with the higher minimum reads false and empty
  while the other keeps its range.
- **Validation.** Rate above max, below min (`min / 2`), NaN →
  `INVALID_VALUE` and no wire frame. Exactly the advertised min and
  exactly the advertised max are accepted, both signs, both axes, and
  send the pinned periods (the period at min follows M0's floor
  multiplier: `FFFFFF` if it is `1.0`). Tertiary →
  `NOT_IMPLEMENTED`. Parked, **at rate 0** → `INVALID_WHILE_PARKED`, no
  frame, AtPark still true. Disconnected → `NOT_CONNECTED`. During a
  slew's completion window (goto bits clear, `Slewing` still true) →
  `INVALID_OPERATION`. Rate 0 with no override → success, no frame,
  Slewing false.
- **Per-axis rule (#1340 item 1), both directions.** Tracking on,
  MoveAxis(Secondary, r) → axis-2 frames only, exactly one `:K1` /
  `:G110` / `:J1` in the log (the ones tracking sent), Tracking true.
  Tracking on, MoveAxis(Primary, r) → the Dec axis receives no
  commands, and a Dec pulse in flight restores on schedule. Tracking
  on, MoveAxis(Primary, r) then 0 → `:I1 <sidereal>` restore, RA never
  stopped. Tracking off, MoveAxis(Primary, r) then 0 → `:K1`, Tracking
  false. Tracking reads the same before and after each round trip.
- **Wire shapes.** Secondary +r → `:K2 :G210 :I2<p> :J2`; −r → `:G211`.
  Primary −r with tracking on → `:K1 :G111 :I1<p> :J1`, then 0 →
  `:K1`, `:G110`, `:I1 <sidereal>`, `:J1`. Primary +r with tracking on
  → a single live `:I1<p>`. Rate change same direction → live `:I`;
  direction reversal → stop, `:G`, `:I`, `:J`. Two rates within one
  stop-and-wait → the first call returns `INVALID_OPERATION`
  (superseded), the second runs. An override start that is refused
  after taking a live-rate RA pulse → RA back at sidereal, Tracking
  unchanged. Rate 0 arriving while the start's stop-and-wait holds the
  axis stopped → the restart sequence, never a live `:I` to a stopped
  motor. Positive Secondary sends the same `:G2` on both sides of the
  pier (no pointing-state inversion); with a southern site latitude
  pinned, the Secondary sense is inverted once.
- **Slewing.** True immediately after MoveAxis(r); false once rate 0's
  restore has landed (the existing `eventually … within` step); true
  with one axis overridden and the other idle; after the first of two
  overrides ends, still true, after the second, false. False during a
  pulse on either axis.
- **Concurrency.** Both axes overridden at different rates; each ends
  independently. PulseGuide on an overridden axis refused, naming the
  override; PulseGuide on the other axis succeeds and IsPulseGuiding
  reads false again while the override still runs. MoveAxis takes over
  an in-flight pulse and the pulse's restore never lands. Slew, sync,
  SetSideOfPier and SetPark refused while an override runs, SetPark
  naming the override and persisting nothing; Park stops the
  override, waits, and parks; Park whose straight route from an
  override-reached pose passes through the zone is refused and the
  mount stays stopped.
- **AbortSlew during overrides** restores each axis like rate 0 and
  leaves Tracking unchanged; Slewing true until the restores land.
  With a Dec override and an RA pulse in flight: Dec restores as rate
  0 (`:K2`), the pulse is cancelled with `:L1`, Tracking false,
  IsPulseGuiding false, Slewing false once the Dec stop confirms. With
  an RA override and a Dec pulse: RA restores to sidereal with Tracking
  unchanged, Dec gets `:L2`.
- **Tracking write during any override** → `INVALID_OPERATION`, no
  frame, Tracking unchanged.
- **Disconnect** with an override active sends `:K` on that axis. A
  transport reconnect under a live client with an override active
  stops the axis on the new link, clears the claim, Tracking false if
  RA, Slewing false.
- **Guards.** With the pinned LST: an RA override toward the band from
  inside the rate-aware band is refused before any frame; one away from
  it is accepted; a change to a higher rate that the gate refuses at
  the new rate → `INVALID_OPERATION`, no frame, the axis stays at its
  current rate; a Dec override with `mech_HA` inside the hard zone is
  refused; inside the floor's rate-aware margin or below it, an
  override that lowers altitude is refused — from just above the floor
  as well as from below — and the raising direction accepted, on both
  axes (Dec toward the horizon; RA with `+Primary` west of the meridian
  and `−Primary` east of it); on the meridian, from just above the floor at a Dec whose upper
  culmination sits there, both RA directions are refused at the
  ceiling rate (the projected travel descends although the slope is
  zero) and accepted at a rate whose travel stays above the floor. An RA override started just outside the band and run toward it is
  stopped at the band edge by the override tick: `:K1`, then Tracking
  false and Slewing false once `:f` confirms the stop, the override
  tick's `warn!`, exactly one `:K1`. A Dec override toward the floor is
  stopped at the floor's margin, and with Tracking on at a margin that
  includes the sidereal term; a raising Dec override near the floor
  with Tracking on is never stopped by the override tick while tracking
  carries the sky down (no `:K2`; any `:K1` is the tracking tick's, at
  the band). Tracking off, a Dec override started just outside the
  hard zone and an RA pulse that carries `mech_HA` inside it → the
  override tick stops the Dec override (`:K2`, confirmed), the pulse
  restores on its own schedule, Slewing false; the same pulse with the
  Dec override started well clear of the zone → no stop. One started
  below the floor in the
  raising direction runs to rate 0 with no guard stop, on either axis,
  as does an RA override started inside the band in the away
  direction. Tracking on, RA override toward the
  band: exactly one `:K1`, from the override tick, not the tracking
  tick. Park after an override whose raw sweep to the park target
  passes through the zone to a target beyond it is refused; one whose
  sweep is clear parks, and one whose configured target sits inside
  the zone still parks, as today. The
  same refusal with the service restarted in between — the seed
  endpoint puts the counter at the override-reached pose and no
  override has run in the new process — pins that the check reads the
  sweep, never a memory of the override; and a park from the ordinary
  post-slew pose still parks.
- **Guard failure modes and the firmware's refusals** are unit tests in
  `mount_device/tests.rs`, not scenarios: the BDD world drives the
  service as a subprocess and its seed endpoint sets only ticks, goto
  targets and the running / goto / initialised flags, while `blocked`,
  `fault_script`, `reject_live_step_period`, `ignore_decelerating_stop`
  and `ignore_instant_stop` are in-process mock knobs. The cases: an
  override on an axis seeded `blocked` gets `:L1` and `:L2` at once
  with no `:K`, a pulse or override on the other axis is cancelled,
  and the claims clear once `:f` confirms; a guard `:K` the
  mock fails (`fault_script`, one entry) is followed by `:L`; both
  failing (two entries) keeps the claim and Slewing true for that tick
  and the next tick's `:K` lands; a `:K` the mock acks but ignores
  (`ignore_decelerating_stop`) keeps the claim until the `:L` the next
  tick sends is confirmed, and Tracking stays true until then; an
  `:L` ignored as well keeps the claim and the watch indefinitely; the
  same two ignored-stop cases driven through rate 0 and through
  AbortSlew: the call returns the error, the claim and Slewing stay,
  the next tick's stop lands once the mock stops ignoring, and only
  then does the claim clear, with Tracking false on RA; the
  staleness stop, with the poll loop held by the manager's own
  `pause_background_polling` so the guard's frames still land,
  asserting the `:K` lands within `S + 1` ticks of the freeze and the
  travel bound that implies; the
  transport-failure latch, with one poll failed at the transport
  (`fault_script`, one entry) and the sample still inside the age cap, so the next tick's
  `:K` lands on a fresh-looking sample; and the
  mount refusing the live change (`reject_live_step_period`), where the
  override starts from rest and ends by stop-and-restart and no
  transport-failure stop follows on the next tick; and the
  start gate's second evaluation, where the snapshot reads clear and
  the mock's ticks are moved into the hard zone before the lock is
  taken, and again where they are moved during the stop-and-wait of a
  start that stops first → refused, no `:G` or `:J` frame, and on the
  second shape the axis is left as `end_failed_start` leaves it (D8).

In `auto_flip.feature`: auto-flip armed, an RA override carries
`mech_HA` across the offset and rate 0 lands after it → the flip still
fires on the next tick; the latch was not spent. The same with a Dec
override running while tracking carries `mech_HA` across the offset. In
`pulse_guide.feature`: a perpendicular pulse during an override
succeeds (the `slewing()` gate is gone), and the #1340 item-2 pins on
the North/South scenarios.

The step vocabulary already exists for nearly all of it: `the mount
should have received exactly {int} {word} frame(s)`, `commands
matching:`, `should not have received command`, `Tracking should be
true` / `false`, `Slewing should be true` / `false` and `Slewing should
eventually be false within {int} seconds`, `I enable tracking`, `the Dec
axis should have received no commands`, the pier-side and LST pinning
steps. New steps: `I move axis {word} at {float} degrees per second`
and `I try to move axis {word} at {float} degrees per second` (a `/`
is reserved in Cucumber expressions, so not `deg/s`), `AxisRates for
{word} should be [{float}, {float}]`, and `the RA axis should have
received no commands`, the twin of the existing Dec step. No new
`Slewing should be …` step: the three that exist cover it.

## Phase M2 — the driver and its guard

Where the work lands, by module, and the one shape decision per module:

- `mount_device.rs` — `PulseGuiding { ra: Option<PulseId>, dec }`
  becomes a per-axis `AxisClaim { Pulse(PulseId) | Override(OverrideId) }`
  slot with the same `get` / `set` / `axes` API, so every existing
  take-over site (`take_pulses`, `stop_taken_pulse_axes`,
  `stop_orphaned_axes`, the guard, abort, disconnect) sees overrides
  through the same calls; `is_active` (behind `IsPulseGuiding`) counts
  `Pulse` claims only. An override claim carries a phase (`Starting` →
  `Running`, flipped under the lock once its `:J` or live `:I` is
  acked), its commanded mode and period for D4's same-regime test, and
  its direction for the D9 gate; the phase routes a rate `0` or a new
  rate that lands during the start's stop-and-wait to the restart
  path. Two latent couplings the #1340 review found are fixed on the
  way: `restart_tracking` hardcodes `Axis::Ra` and
  `stop_after_failed_restore` keys `Tracking` off `plan.restore` rather
  than the axis; both become axis-keyed.
- `mount_device/override.rs` (new) — `start_override`, `change_rate`,
  `end_override` (rate 0 and `AbortSlew`), with the pulse's burst
  discipline: hold `axis_ownership` for a burst, release across a
  stop-and-wait, re-check the claim before the next frame. The restore
  reuses the pulse's `restore` / `escalate` / `stop_after_failed_restore`
  ladder rather than copying it; those move to a shared helper if the
  borrow shapes allow, else the pulse's functions take the claim kind
  as a parameter. The claim is released only after the restore lands;
  an unconfirmed stop hands it to the override tick, which clears it on
  confirmation (D5), and the guard's own stop clears it the same way,
  because it never restarts.
- `mount_device/telescope.rs` — `can_move_axis`, `axis_rates`,
  `move_axis`; `slewing` reads override claims; `pulse_guide`'s
  pre-check becomes the goto reservation plus goto bits, and
  `begin_pulse`'s per-axis refusal names an override; `set_tracking`
  refuses while any override claim is held; `abort_slew` ends overrides
  as rate 0 does; `park` takes overrides over with the override
  stop-and-wait budget and runs the raw-sweep RA path check on every
  park; `sync_to_coordinates` and `set_side_of_pier` refuse under the
  lock they already take, and `set_park` refuses and captures under it.
- `mount_device/inherent.rs` — inside the `axis_ownership` block that
  acquires the slew reservation, before `try_acquire`: any override
  claim → `INVALID_OPERATION`.
- `mount_device/device.rs` — disconnect stops overridden axes. Nothing
  runs on a transport reconnect (D10).
- `mount_device/tracking_guard.rs` — the override tick (D9), first in
  `guard_loop_tick`, with its confirm-then-release stop and its
  cross-tick `:K` → `:L` escalation; the tracking tick skips RA while an
  RA override claim is held and the auto-flip tick is inert while any
  override claim is held; the projected sample and the staleness rule
  while any override claim is held.
- `config.rs` — no new key. The firmware ceiling, its safety factor and
  the override stop-and-wait budget are named constants set from M0's
  record, keyed on the mount code the identity gate reads; the
  margin-cap clamp runs at connect from the existing `command_timeout`
  and `polling_interval`, and `AxisRates` reports the clamped value.
- `coordinates.rs` — `move_axis_step_period(sidereal_period, factor)`
  is `pulse_guide_step_period` renamed to what it is, with both
  callers. The 24-bit clamp lives in the `MoveAxis` caller only: the
  pulse keeps refusing a period outside 24 bits with `INVALID_VALUE`,
  as the design doc pins.
- `manager.rs` — the override's staleness cap is its own constant
  (D9), so no projection-age accessor; a transport-failure counter the
  poll loop and the sends bump on **transport** errors only (a firmware `!` refusal or a protocol error is an answer from a live link and never counts), which the override tick samples at claim start and
  compares each tick, so a glitch shorter than the age cap still
  latches the stop (D10). Otherwise nothing new unless M4: the commanded-period record
  already carries the override's `:I`, so RA reads during an override
  project at the override rate; and both restore shapes re-record
  sidereal or zero it, so `begin_pulse`'s sidereal gate is sound after
  a restore.
- `transport/mock.rs` — high-speed ratio re-seeded to the hardware's
  `1` (the gearing unit tests pass their own ratio and lose nothing).
  The guard's `blocked` branch is tested by seeding `blocked` on the
  mock state as the slew-watcher tests already do; the mock does **not**
  gain a rate threshold that sets it, because no such firmware rule has
  been observed and inventing one is the mock-from-the-same-spec trap.
  Fast-mode gearing from M0 if M4.
- `tests/conformu_integration.rs` — the inline mock config gains
  `"unpark_from_ap_position": "ap_park_3"`; the expected report stays
  clean.
- Documents that state what is implemented: the design doc's planned
  (M2) markers from M1 come off in this commit, `docs/workspace.md`'s
  service row adds `MoveAxis` to the implemented `ITelescopeV3` subset,
  and `device_metadata.feature`'s description drops it from the
  not-supported list.

Per the development workflow, no driver code is written before the M1
scenarios exist; the scenario-level `@wip` tags come off in the commit
that lands M2.

## Phase M3 — hardware validation

1. ConformU `alpacaprotocol` + `conformance` on the field rig against a
   packaged nightly, the way the
   2026-09-26 record was made, filed as a `docs/validation/` record
   with its row in the index. The MoveAxis Primary / Secondary groups
   must read clean: zero-rate stop, below-min and above-max refused,
   ±min and ±max legs with `Slewing` true then false, tracking state
   retained in both states, and the three timed members (the first
   rate `0`, the `+min` and `+max` starts) within the 1 s target.
   The sweep the operator plans is one ceiling leg's travel plus the
   measured coast, from the pose the AbortSlew test leaves. **The
   all-zero rule needs #1371 fixed on the build under test**; it is the
   successor of the cross-axis RA-read findings that made the
   2026-09-26 record scoped. If it is not, the run is filed as a scoped
   record naming #1371 only on the hardware-validation skill's terms:
   every remaining finding is #1371's, in the PulseGuide cross-axis
   checks it already owns, **none is in a MoveAxis group**, and the
   alert and timing counts are zero; the decision is recorded on #1340
   and the README and index row say so — decided 2026-10-03: file
   scoped rather than wait, so the MoveAxis groups and the client check
   get their hardware evidence now. A finding inside a MoveAxis group,
   whatever its cause, makes it a failed run that gets an issue, not a
   record; the client check below still runs and its results go on
   #1340.
2. The clients: N.I.N.A.'s telescope-tab paddle in both directions on
   both axes at two rates, and TPPA automatic mode to a solved
   alignment, against the driver. TPPA is the use case that motivated
   this; a ConformU pass that TPPA cannot use is not done. Results go
   in the record's README, as the exposure-path tests do in the camera
   records.
3. The design doc's §Real-hardware validation gets the entry.

## Phase M4 — the fast regime, only if M0 finds one

Taken only when experiment 2 shows Tracking-Fast reaching rates Slow
cannot and a client wants them. Then: `manager.rs` takes the Fast
gearing from M0's record (a parameter override, never `:g`), with a
BDD pin that an RA read during a Fast override projects at the
measured rate; `transport/mock.rs` models whichever Fast live-`:I`
outcome M0 recorded (refused, as INDI assumes, or applied), so a
Fast-mode scenario cannot pass on the mock's current unconditional
acceptance; D4 gains the Fast regime's live-change rule from the M0
trial, or the rule that Fast overrides always stop first; the design
doc's commands table and `AxisRates` text follow.

## Risks and what bounds them

- **A stall is invisible from the wire, and corrupts the frame.** The
  count says the axis moved; it did not. The ceiling constant with
  its safety factor is the only mitigation; `blocked` is defence in
  depth the firmware has never been seen to provide; a suspected stall
  needs a re-sync, and M1's design-doc text says so. The constant is
  mount-wide, not per rig: a stall is load-dependent and no
  per-installation number could be verified from the wire (D3), so M0
  measures the ceiling with the mount carrying the heaviest imaging
  train it will be given (a stated precondition, the mass in the
  record), the safety factor is taken from that record, and M3 runs the
  ceiling legs on the field rig's own payload before any record is
  filed. A load neither run has carried is the case the reserve
  `move_axis` block in D3 exists for, and the design doc says so where
  it states the constant.
- **Tracking-Fast is the Phase 4 runaway mode.** An override in that
  mode is by design what the codec bug did by accident. The start gate
  and tick (D9), the reconnect and disconnect stops (D10), the abort
  path and the probe's cut rule bound it, and `CanMoveAxis` never flips
  without the guard.
- **Coast after a stop from speed.** 3.77° were measured after an
  instant stop from 5.1°/s. The guard margin, the probe's travel bounds
  and the stop-and-wait budget all carry M0's coast term; none reuses
  the 2 s and 4 ms the sidereal paths were sized on.
- **ConformU's ceiling legs.** Four 2 s legs at the ceiling per axis,
  two each way, from wherever the AbortSlew test left the mount. The
  mock config starts from Park 3; on hardware the record's README names
  the start pose and the operator plans the sweep, as the 2026-09-26
  record did for the flip test.
- **#1311 gains an operation, and closes its own window.** The start
  gate is a pose-dependent decision, so like slew and park it can be
  made stale by a sync between the decision and the commit; unlike
  them, D9 evaluates it again under `axis_ownership` on a fresh read
  immediately before the motion burst, after any stop-and-wait, so an
  override never starts on a verdict a sync invalidated. The disconnect and post-abort holes #1311 lists
  are neither widened nor closed by this plan, and #1311 stays open.
- **Projection during an override.** `RightAscension` carries the poll
  sample forward at the commanded rate; at 3°/s a 200 ms poll is 0.6°
  of carry, so the read-instant arithmetic in §Encoder samples and the
  read instant is exercised harder than tracking ever did. The BDD
  reads of RA during an override pin the carry; the hardware record
  confirms it. Under M4 the Fast gearing the projection uses comes from
  M0's measurement, never from `:g`.

## Out of scope

- Custom tracking rates (`RightAscensionRate`, `DeclinationRate`,
  lunar / solar `TrackingRate`) — still deferred, though an override is
  the primitive they would build on.
- The Tertiary axis.
- Alt/Az `MoveAxis` semantics (the GTi is a GEM).
- A tracking-time altitude guard. Today tracking runs through the
  floor unguarded — the floor gates slew and sync targets, and the
  tracking-time guard watches the band — and this plan neither adds
  one nor widens that exposure; it is its own follow-up.
- The planetarium-bridge facade's `CanMoveAxis`, which stays `false`.
- An `rp` or `ui-htmx` paddle; nothing in `rp` needs the method.

## References

- Issue [#1340](https://github.com/rusty-photon/rusty-photon/issues/1340)
  and the assessment on it; related #1299 (live `:I`), #1300 / #1332
  (counterweight-up Dec direction), #1311 (`axis_ownership` scope),
  #1344 (nightly ConformU re-entry), #1371 (the RA read-instant finding
  that gates an all-zero record).
- [ASCOM ITelescope V4 — `MoveAxis`](https://ascom-standards.org/newdocs/telescope.html#Telescope.MoveAxis):
  "A call with Rate = 0 is required to stop motion and return to the
  previous tracking state of that axis"; "A MoveAxis() operation on one
  axis must not affect any of the other axes"; "Do not use this method
  to effect guiding." And `AbortSlew`: "In the case of MoveAxis() …
  Tracking must be returned to the state before the slew stopped."
- [ConformU#32](https://github.com/ASCOMInitiative/ConformU/issues/32),
  Peter Simpson's comment of 2026-09-27.
- ConformU 4.5.0 `TelescopeTester.cs`: `TelescopeMoveAxisTest` (the
  sequence M3 must pass, `MOVE_AXIS_TIME = 2000` ms),
  `TelescopeAxisRateTest` (negative, zero-zero, overlap and duplicate
  checks on `AxisRates`), and the parked-exception test that sends
  rate `0` to a parked mount.
- ASCOM Alpaca Simulators `TelescopeHardware.cs` (OmniSim, which rp's
  BDD runs against): positive `MoveAxis` rates increase the mechanical
  angle on both axes in the northern hemisphere and the Secondary sense
  is inverted once in the southern; `AbortSlew` clears the rates and
  leaves tracking as it was; a `Tracking` write during a move is applied
  when the move ends. It also suspends **all** tracking while any
  `MoveAxis` rate is set — RA stops tracking during a Dec-only move — so
  it is not a reference for rule 3; the in-tree mock is.
- INDI eqmod `MoveWE` / `MoveNS` and `Skywatcher::SlewRA` / `SlewDE`:
  rate presets 1–800 × sidereal, Slow below 128 ×, Fast above with the
  period divided by the high-speed ratio, a live `:I` refused in Fast,
  tracking restarted on stop from the remembered state, a mechanical
  sign with a static user inversion.
- [`docs/services/star-adventurer-gti.md`](../services/star-adventurer-gti.md)
  §PulseGuide lifecycle (the restore ladder and lock discipline this
  reuses), §Tracking-time safety guard, §Safety envelope, §Altitude
  floor, §Phase 4 findings (the high-speed ratio, the runaway, the
  audible stall), §Encoder samples and the read instant.
- [`docs/references/skywatcher-motor-controller-command-set.md`](../references/skywatcher-motor-controller-command-set.md)
  — where M0's measurements go.
- [`docs/skills/hardware-validation.md`](../skills/hardware-validation.md)
  — the record format, the scoped-record rule and the version gate,
  for M3.

# star-adventurer-gti: runs on the rig, 2026-05 to 2026-10

A record, moved whole from `docs/services/star-adventurer-gti.md` on
2026-11-12: the dated log of what was run against a physical Star
Adventurer GTi — ConformU runs, scripted checks through the Alpaca
endpoint, and wire probes with the service stopped — and what each
run settled. The design document states what the hardware does; the
ConformU records themselves are in
[`docs/validation/`](../../validation/README.md).

## The runs, newest first

This list stood under
[star-adventurer-gti.md § Real-hardware validation](../../services/star-adventurer-gti.md#real-hardware-validation).

- **2026-10-07 — sync around an abort's coast (for #1311)** on the
  field rig: packaged arm64 nightly of `935e547`, through the running
  service's Alpaca endpoint with rp stopped, every slew target at
  Dec +60°. A scripted run of 26 checks.
  - **The coast reads as tracking:** after `:L`, `:f` read `=111` on
    both axes, not the goto they were running. Sync's check for a
    running goto therefore let two syncs through mid-coast, and each
    wrote counts 200–240 ticks behind where the axes were. The check now
    also counts an axis running with no tracking rate the driver
    commanded; see [§Safety stop at startup](../../services/star-adventurer-gti.md#safety-stop-at-startup).
  - **A slew straight after `AbortSlew`** is accepted once the coast
    ends (1.55 s after the abort) and lands 2.1 s of RA from its target.
  - **A disconnect and reconnect mid-slew** (1 ms apart) leave the goto
    running and reading as a goto; sync is refused until it ends.
  - **A flip aborted 6.9 s in**, once `SideOfPier` read East, came to
    rest 4.7° past the pole. A sync there kept pierEast, and a slew
    straight back from that pose landed.
  - **`SetSideOfPier`** latched the pointing read just before it.
  - **The fix, run again on the rig before it merged** (a branch build
    swapped in for the packaged binary): 29 of 29 checks passed. Syncs
    0.004 s and 0.6 s into an abort's coast, and 0.004 s into an aborted
    flip's coast, were refused. Syncs while tracking and during North
    and East guide pulses were accepted, though RA read the same `=111`
    while it tracked. Every accepted sync wrote the count it had read,
    to within a tick.

  There is no record: this was not a ConformU run.

- **2026-10-06 — RA pulse edge-step trim defaults on the field rig**
  ([record](../../validation/2026-10-06-star-adventurer-gti-gti-rig-pulse-trim/README.md)).
  - **Build:** packaged arm64 nightly of `7700ee97`, with no
    `ra_pulse_edge_steps` override.
  - **ConformU:** 4.5.0 over the production TLS+auth endpoint.
  - **Verdicts:** `alpacaprotocol` 0 / 0. `conformance` 0 errors / 1 issue /
    0 alerts / 0 timing issues.
  - **The trim defaults hold:** all eight East/West legs pass at both
    edge-step levels (mech −9 h low, −3 h high), the worst 0.03 s against
    0.07 s.
  - **The one issue:** a cross-axis RA read during the HA +3 North pulse
    (+0.10 s). A replay of the wire trace puts it down to the single-sample
    `RightAscension` read, the residual of #1371.
  - **Filing:** a **scoped record** by decision. #1371 was then closed as
    not planned, which accepts the finding as a limitation of this mount;
    no clean run is planned.

- **2026-10-05 — RA pulse edge steps across the arc (for #1362)** on
  the field rig, packaged arm64 nightly of `7d6f73b`, through the
  running service's Alpaca `PulseGuide`, with the edge steps read from
  its TRACE wire log. Two sessions: 176 East/West pulses at 11 poses,
  Dec 80, mech HA −10.5 to −1.5 on both pier sides; then 160 pulses
  over 20 visits that crossed the pose with the direction and length
  of the slew that reached it. They settled that the board's edge
  steps over a pulse take one of two levels, about +0.65 and +2.4
  ticks, picked by the RA axis position, rather than one constant per
  rig; the `ra_pulse_edge_steps` defaults moved to their midpoint. The
  tables are under "What the rig measured" in
  [§PulseGuide lifecycle](../../services/star-adventurer-gti.md#pulseguide-lifecycle). There is no record:
  this was not a ConformU run.

- **2026-10-03 — goto stop-coast probe (for #1287)** on the field rig,
  service stopped, starting from `ApPark3`, using
  `examples/probe_stop_coast.rs` over USB. 28 gotos of 30° were each
  stopped 12° in: Dec away from and toward the pole, RA both ways, and
  both axes at once with the safety-stop sequence. All 28 completed, and
  all 38 positioning and return gotos landed exactly on target. It
  settled:
  - `:L` and `:K` stop a goto identically. Dec from 5.01°/s runs on
    3.76° and reads stopped after 1.50 s. RA from 4.24°/s runs on 2.69°
    and reads stopped after 1.27 s. A constant deceleration fits both to
    the millisecond.
  - For about a second after `:f` first reads stopped, the count moves
    back 9–20 ticks, then holds exactly.
  - `blocked` never appeared.
  - `AXIS_STOP_TIMEOUT`'s 2 s leaves about 0.5 s of margin over a Dec stop
    from goto speed.

- **2026-09-28 — live step-period probe (P0 for #1299)** on the field
  rig, service stopped, RA tracking at the park pose, using
  `examples/probe_live_step_period.rs` over USB. It settled the firmware
  behaviour the live-rate pulse rests on:
  - a live `:I1` is accepted and applied at once (240/240);
  - a stray `:I1` never starts a stopped axis;
  - `:K1` stops a tracking axis in ~4 ms;
  - every rate change adds a small forward step, which `ra_pulse_edge_steps` first defaulted to.

  The measurements are listed under "What the rig measured" in
  [§PulseGuide lifecycle](../../services/star-adventurer-gti.md#pulseguide-lifecycle), and the full tables
  are on issue #1299. There is no record: this was not a ConformU run.

- **2026-09-26 — counterweight-up PulseGuide on the field rig**
  ([record](../../validation/2026-09-26-star-adventurer-gti-gti-rig/README.md)).
  Packaged arm64 nightly of `6eaa9b70` on the Raspberry Pi 5 rig,
  ConformU 4.5.0 with meridian flips switched on (then opt-in) at latitude 32.7°,
  over the production TLS+auth endpoint. `alpacaprotocol` 0 errors /
  0 issues; `conformance` 0 errors / 11 issues / 0 alerts / 0 timing
  issues. What it settled: Dec guide pulses move the right way in
  **both** pointing states — at HA +3 and +9, reached through the pole
  (Dec encoder +135° / +95°), North +37.9″ and South −37.9″ against
  ±37.6″, the same as counterweight-down — so #1300's fix is measured,
  not derived. The through-wrap meridian flip (`SideOfPier Write`) and
  the pointing-state reads at HA ±3 / ±9 pass. The 11 issues are all
  RA: seven #1299 offsets and four cross-axis RA readings during Dec
  pulses (issue #1334). Filed as a **scoped** record — it does not meet
  the all-zero rule while those two RA defects are open, and its README
  says so; a clean two-suite record is owed once **both** #1299 and
  #1334 have landed, since either one alone still leaves issues on the
  board.
- **2026-09-21 — #1295 PulseGuide rate fix** on the rig from a branch
  build: Dec North/South 5 s at 37.5–38.4″ (was 46.9 / 47.3″) and the
  RA offset at 2 / 5 / 10 s. Results on
  [PR #1298](https://github.com/rusty-photon/rusty-photon/pull/1298);
  no record, because it was not a two-suite ConformU pass.
- **2026-05-16 — meridian flip, AP Park 1–5 traversal** (predates the
  record trail, no ConformU artefacts): see
  [§Hardware validation](#the-ap-park-traversal-of-2026-05-16) under Meridian flip.

## The AP park traversal of 2026-05-16

This passage stood under
[star-adventurer-gti.md § Hardware validation](../../services/star-adventurer-gti.md#hardware-validation),
in § Meridian flip.

Flips were opt-in (a `flip_policy.enabled` switch, default `false`)
until the through-wrap flip and flip-back paths had been verified
end-to-end on a GTi; with that done they are always on (see
[§Flip policy](../../services/star-adventurer-gti.md#flip-policy)).

**2026-05-16, lat 32.7°N (San Diego).** The AP Park 1–5 sequence ran
end-to-end:

1. `Park 3 → Park 2`: small Dec slew (−90° to celestial equator).
2. `Park 2 → staging (HA = 0, Dec = −57.3°)`: pre-flip pierWest small
   slew; no routing change.
3. `SetSideOfPier(East) → Park 4 N`: through-wrap flip slew — RA
   −180° (saddle west → east) + Dec +294° CW (through wrap, past
   NCP, ending past-pole at `dec_encoder ≈ −122.78°`).
4. `Slew to (LST + 12 h, Dec = +57.3°) → Park 5 N`: flip-back over
   the saddle-east wrap. Wire issued was RA **−3,631 ticks CCW**
   (canonical short path, ~0.024° of polar-axis rotation) + Dec
   −180° CCW via NCP. The earlier sign-blind heuristic issued a
   +3.625 M-tick CW full revolution instead and the operator
   powered the mount off mid-sweep — see
   [§"Through-wrap slew routing" above](../../services/star-adventurer-gti.md#through-wrap-slew-routing)
   for the path-aware fix.
5. `Park → Park 3 N`: clean unwind.

All five poses confirmed visually. The dec axis's analogous routing
helper (`flip_slew_dec_delta`) was not exercised on its
sign-blind edge case in this session (current dec was on the
`+cpr_dec/2` side of the wrap where the old heuristic happens to be
correct); a follow-up issue tracks porting the path-aware fix to that
function.

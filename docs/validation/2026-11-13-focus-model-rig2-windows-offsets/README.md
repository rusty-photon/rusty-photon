# focus-model on rig2, Windows: filter offsets (`determine_filter_offsets`)

The rig night of 2026-11-04
([record](../2026-11-04-focus-model-rig2-windows/README.md)) checked a
hand-entered offset on a filter change, and left
`determine_filter_offsets` under *Not covered*. On 2026-11-13 (UTC) the
procedure ran twice on rig2 under one clear sky, each run three rounds
over all seven filters on the wheel against L:

- **Run 1** walked the wheel order, L R G B H S O.
- **Run 2** reversed the order after L, L O S H B G R, to show how much
  of run 1's result came from the order the filters were measured in.

All 42 sweeps confirmed at their first attempt, R² 0.973–0.997, and both
runs wrote their offsets and put the rig back as focus-model.md §
`determine_filter_offsets` describes. That includes the three narrowband
filters, at the train's one 4 s exposure. Run 2 predicted every start
from run 1's measured offsets across a filter change: each predicted start
lay within 8 steps of the fitted focus, inside the critical focus zone
(CFZ).

Focus drifted about 1 step a minute through run 1, and 0.5 a minute
through run 2. The procedure measures the reference first in every round,
so each filter's difference also carries the drift since its round's L
sweep: up to about 12 steps (observation 1). The offsets now on the rig
were written by hand afterwards, at the owner's choice. They are the mean
of the two runs, each corrected for that drift.

This is not a ConformU run: no driver's Alpaca behaviour was under test.

## What was tested

| | |
|---|---|
| Commit | [`21bfe94d`](https://github.com/rusty-photon/rusty-photon/commit/21bfe94d) (`main`), published by the nightly run [42369066171](https://github.com/rusty-photon/rusty-photon/actions/runs/42369066171) as `0.1.0+nightly.202611071139.g21bfe94` |
| Build | The published nightly's suite MSI, installed in place on 2026-11-07 (`0.1.0.26311`) |
| Host | `rig2`: Windows 11 Pro (x64) at a remote observatory, the services under the SCM, TLS and the observatory credential throughout. Pegasus Unity was not running, so `upbv2-driver` switched the UPBv2 hub's outputs and was stopped again afterwards. Optec's FocusLynx Alpaca server ran as a scheduled task at boot |
| Train | `main`: 530 mm / 106 mm refractor (f/5, no obstruction); DSD FP2 flat panel as the cover; Optec FocusLynx through Optec's own Alpaca server; QHY600M `QHY600M-3b1ea54688eb9f9d7` (3.76 µm pixels, 1.463″/px, uncooled) with its 7-slot wheel, L R G B H S O |
| `rp` | `focus-model` registered as a tool provider as focus-model.md § Registration in `rp` gives it. `focusers[focuslynx].microns_per_step` 2.1. Wavelengths for H (656.3), S (672.4) and O (500.7); L, R, G and B carry none, so the sweep takes 550 nm for them |
| `focus-model` | `trains.main`: 4 s frames, `min_area` 6, `max_area` 5000, `threshold_sigma` 8, one frame per step, `min_fit_points` 5, `confirmation_tolerance` 0.25, `max_attempts` 2, `step_size` and `half_width` unset (derived); sweep defaults (`end_ratio` 4, 9 points, 2.5″ seeing) |
| Record as found | reference L, offsets `{"L": 0}`; a temperature coefficient of 14.745 steps/°C fitted from 26 L runs over 9.8 °C, with `coefficient_offsets` `{}`; last good L 29487 at 6.4 °C and R 29623 at 15.3 °C; 34 runs |
| Procedure | The train was powered only once the Sun was below −12° and `rp`'s `get_safety_status` read the roof open. The cover was opened and a 4 s L frame checked for stars before each run: 580 at run 1's start (median HFR 4.0, defocused at the previous night's 6.4 °C focus), and 2221 at run 2's (median HFR 1.05). Afterwards the cover was closed and every switch put back as found. Run 1 was run unattended by a scheduled session following a written runbook, run 2 by hand from the same scripts, both in one roof opening |

## Results

`determine-filter-offsets-results.json` holds, for each run:
- the call;
- `get_focus_model` before and after;
- the result as `rp` relayed it;
- the 21 progress notifications it streamed.

It also holds the final `set_focus_offsets` call and the model it left.
`focus-runs.json` holds the 42 runs the two procedures recorded, as
`get_focus_runs` returned them, curve points included, oldest first.

### The positions

Run 1, 00:43–01:22Z, focuser probe 13.6 → 12.9 °C:

| round | L | R | G | B | H | S | O |
|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | 29583 | 29596 | 29572 | 29569 | 29549 | 29550 | 29519 |
| 2 | 29568 | 29580 | 29555 | 29554 | 29535 | 29536 | 29507 |
| 3 | 29557 | 29568 | 29543 | 29543 | 29522 | 29526 | 29493 |

Run 2, 01:31–02:10Z, 12.4 → 11.8 °C, measured in the order L O S H B G R:

| round | L | O | S | H | B | G | R |
|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | 29544 | 29487 | 29519 | 29513 | 29531 | 29525 | 29550 |
| 2 | 29537 | 29480 | 29513 | 29504 | 29521 | 29517 | 29541 |
| 3 | 29530 | 29473 | 29504 | 29498 | 29516 | 29513 | 29538 |

Every sweep took about two minutes: 8–9 accepted samples, step 19–33,
half width 75–129. The focused HFR was 0.73–1.18 px and the measured wing
slope 3.70–5.02 px per 100 steps, against a predicted 3.949. The reference
frames listed 932–2153 stars through L, R, G and B, and 48–83 through H, S
and O.

### The offsets

| filter | run 1: differences → written | run 2: differences → written | CFZ (steps) |
|---|---|---|---:|
| R | +13, +12, +11 → **+12** | +6, +4, +8 → **+6** | 32.0 |
| G | −11, −13, −14 → **−13** | −19, −20, −17 → **−19** | 32.0 |
| B | −14, −14, −14 → **−14** | −13, −16, −14 → **−14** | 32.0 |
| H | −34, −33, −35 → **−34** | −31, −33, −32 → **−32** | 38.1 |
| S | −33, −32, −31 → **−32** | −25, −24, −26 → **−25** | 39.1 |
| O | −64, −61, −64 → **−64** | −57, −57, −57 → **−57** | 29.1 |

The CFZ is each filter's `get_sweep_plan` `cfz_steps`. Within a run, a
filter's three rounds lie within 4 steps of each other (R in run 2), and
within 3 for every other filter. Between the runs,
R differs by 6 steps, G by 6, S by 7 and O by 7.

### The checks

What focus-model.md § `determine_filter_offsets` commits the procedure
to:

| | Run 1 | Run 2 |
|---|---|---|
| Every sweep is `focus_train`'s body, recorded as a run, updating its filter's last good focus | **pass**: 21 runs (34 → 55); last good for all seven filters | **pass**: 21 runs (55 → 76); last good replaced for all seven |
| A round walks the reference first, then the `filters` list in its order | **pass**: L R G B H S O | **pass**: L O S H B G R, as asked |
| Each offset is the median of its rounds' differences against that round's reference; the reference is 0 | **pass**, for every filter in the table above | **pass** |
| Writes the reference and the offsets; the result's `offsets` is the record | **pass**: `offsets_written: true`, and `get_focus_model` read the same | **pass**: the same |
| Restores the filter selected before the call and moves the focuser to its last measured position | **pass**: L, at 29557 (round 3's L) | **pass**: L, at 29530 |
| A coefficient fitted from one filter survives an offsets write (§ `calibrate_temperature`) | **pass**: 14.745…, `coefficient_offsets` `{}` | **pass**; also after the final `set_focus_offsets` |
| Each sweep's predicted start (`prediction.start`) | 16 of the 21 had one; each lay 2–13 steps above the fitted focus, inside the CFZ | **all 21** had one, from run 1's offsets across a filter change; each lay between 8 steps below and 7 steps above the fitted focus, inside the CFZ |

No filter was unmeasured, and no sweep needed a second attempt.

In run 1, the first L sweep's start came from the last L focus (29487 at
6.4 °C, 2026-11-12 11:44Z) plus the coefficient's term over 7.2 °C:
+106.2 steps, so 29593. It fitted at 29583, 10 steps from the start: inside
the CFZ's half of 16. Round 1's G to O had no last good focus and no
offset, so no prediction: each started where the previous filter's sweep
left the focuser. In rounds 2 and 3 each filter predicted from its own
previous focus, and every predicted start lay above the focus the sweep
then found: the drift in observation 1.

In run 2, every predicted start was the previous sweep's focus, moved by
the difference of the two filters' run-1 offsets and, in seven of them, by
a temperature term (−1.5 steps, or −7.4 for the first). Each step from L to
O predicted with −64 and started 7–8 steps below the fitted focus. Each
step from G to R predicted with +25 and started on it. Nine of the 21 moves
were skipped, the predicted start lying within half a CFZ of the focuser.
Those sweeps began where the focuser stood, 3–13 steps from the focus they
found.

## Observations

1. **Focus drift within a round reaches the offsets.** focus-model.md says
   so: a round's differences are against a reference refocused inside it,
   "so the drift a round cannot outrun is the only one that reaches an
   offset". Here that drift was large:
   - **Run 1:** L moved −26 steps in 26 minutes. That is −1.12 steps/min
     between rounds 1 and 2, and −0.86 between rounds 2 and 3.
   - **Run 2:** L moved −14 steps in 26 minutes, −0.55 and −0.51 steps/min.
   - **The effect:** a filter measured t minutes after its round's L
     carries about t × that rate. In run 1 the filters came 2 (R) to 11
     (O) minutes after L; in run 2, 2 (O) to 11 (R).
   - **How the two runs differ:** mostly in the direction their timing
     predicts. R and G read lower in run 2, where they came later; H, S
     and O read higher, where they came earlier. B, measured 6–7 minutes
     after L in both runs, reads the same.

   Correcting each difference against L interpolated linearly to the
   moment its sweep started, between that round's L and the next, brings
   the two runs within 4.7 steps of each other. Round 3 extrapolates the
   round 2→3 rate. No gap the correction leaves has run 2 the higher: they
   run from 0 to −4.7 steps, which a linear drift of L does not account for.
   Drift that is not linear, or offsets that change with temperature,
   would leave such a remainder.

   | filter | run 1, corrected rounds → median | run 2, corrected rounds → median | run 2 − run 1 | mean, rounded: **written** |
   |---|---|---|---:|---:|
   | R | +15.7, +13.9, +12.8 → +13.9 | +11.9, +10.0, +13.5 → +11.9 | −2.0 | **+13** |
   | G | −6.2, −9.5, −10.4 → −9.5 | −14.1, −15.3, −12.5 → −14.1 | −4.7 | **−12** |
   | B | −7.1, −8.8, −8.8 → −8.8 | −9.1, −12.4, −10.5 → −10.5 | −1.7 | **−10** |
   | H | −24.9, −26.2, −28.1 → −26.2 | −28.0, −30.3, −29.3 → −29.3 | −3.1 | **−28** |
   | S | −21.9, −23.7, −22.8 → −22.8 | −22.8, −22.1, −24.1 → −22.8 | 0.0 | **−23** |
   | O | −51.0, −51.3, −54.3 → −51.3 | −55.7, −55.9, −55.9 → −55.9 | −4.6 | **−54** |

   The owner chose the last column over either run's own result, and it
   was written with `set_focus_offsets` at 02:33Z. The correction is this
   record's arithmetic over `focus-runs.json` (each run's `at` and
   `position`), not something focus-model computes. Run 2's own
   predictions point the same way: its starts across a filter change used
   run 1's uncorrected offsets, and the step from L to O, whose offset
   carried the most drift, started 7–8 steps below focus each round.
   Having the procedure correct for this drift itself is
   [#1772](https://github.com/rusty-photon/rusty-photon/issues/1772).

2. **Early in the night, focus moved 2.4–3.7 times as far as the probe's
   temperature predicts.** Over the 26 minutes of run 1's L sweeps the probe fell 0.5 °C (13.6 → 13.1), for
   −26 steps of focus. At 14.745 steps/°C the probe's reading accounts for
   7 of them. Run 2's L moved −14 steps over 0.4 °C (12.4 → 12.0), where
   the coefficient accounts for 6. So the predictions inside run 1 that
   carried a temperature term (−2.9 to −5.9 steps per round) still
   started 6–13 steps above where focus had gone. The coefficient's own
   fit pools runs taken through whole nights. These runs do not tell a
   probe that lags the optics apart from a coefficient that does not fit
   this hour of the night, or from drift with another cause.

3. **The narrowband filters focus at the broadband exposure.** At 4 s the
   H, S and O reference frames listed 48–83 stars. Every narrowband sweep
   accepted 8–9 samples, confirmed and fitted R² 0.974–0.996, with focused
   HFRs of 0.73–1.00 px. The sweep sized them for their wavelengths:
   CFZs of 38.1, 39.1 and 29.1 steps, against 32.0 at 550 nm.

4. **The offsets fall into two families, broadband and narrowband.** In
   the written offsets:
   - **The narrowband filters focus higher the longer their wavelength:**
     O (500.7 nm) −54, H (656.3 nm) −28, S (672.4 nm) −23.
   - **Among the broadband filters R, at +13, sits above G (−12) and B
     (−10).** B sits 2 steps above G, less than the 4.7 by which the two
     runs' corrected values for G differ, so these runs do not order G and
     B.
   - **L sits near the mean of R, G and B** (−3).
   - **The narrowband filters sit 36–44 steps below the broadband ones at
     like wavelengths.** H and S, at wavelengths a red filter passes, sit
     41 and 36 steps below R. O, at the edge between blue and green, sits
     44 below B and 42 below G.

   Each run's own uncorrected offsets show the same narrowband order, R
   above G and B, and a gap of 31–51 steps between the families. G and B
   change places between the runs (run 1 G −13, B −14; run 2 B −14,
   G −19). A refractor's
   colour curve does not put H 41 steps from R at so nearly the same
   wavelength, and this record does not establish what does.

5. **R − L against the hand-entered runs.** The corrected R offset is +13.
   The hand-entered runs had measured R at L + 14 and L + 15 on 2026-11-04,
   +17 on 2026-11-05 and +9.8 on 2026-11-11. Each was an R sweep taken
   minutes after its L sweep, with the drift of its moment.

6. **The record left on rig2:**
   - reference L, offsets `{"L": 0, "R": 13, "G": -12, "B": -10, "H": -28, "S": -23, "O": -54}`;
   - the coefficient 14.745 steps/°C unchanged, `coefficient_offsets` `{}`;
   - last good for all seven filters from run 2's round 3, 11.8–12.0 °C;
   - 76 runs;
   - the train on L at 29530.

## Not covered

- The **shared walk** and the **`refocus-on-temperature`** rule: still
  `pier1`'s, as in the 2026-11-04 record.
- **`determine_filter_offsets` on a guiding train** (focus-model.md §
  Training runs on a guiding train): rig2 has none.
- **A refit of the coefficient over several filters.** The coefficient in
  the record was fitted from L alone. With all seven filters now carrying
  an offset, a `calibrate_temperature` call would put every filter's runs
  on L's scale and fit them too: the 36 non-L sweeps of these two runs and
  the earlier R runs. It was not run.

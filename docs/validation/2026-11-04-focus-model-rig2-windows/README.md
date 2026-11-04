# focus-model on rig2, Windows (the S4 rig night of #1683)

The rig night [#1683](https://github.com/rusty-photon/rusty-photon/issues/1683)
asks for before the focus-model plan's S7: the provider focusing a real
train under a real sky. Its first two runs by night came on rig2, whose
train has a filter wheel but no mount or guider, so three of the gate's
five checks could run there:

- **A — the derived sweep** against the hand-tuned `auto_focus` block it
  replaces;
- **B — a predicted start** from the remembered focus, landing inside the
  sweep's band;
- **C — a hand-entered offset** applied on a filter change.

All three passed on the second night's runs. The first night's runs
passed B and C, and only half of A: every run confirmed on the derived
grid, but the ends of the sweep sat at 7–11× the best HFR instead of the
4× the derivation aims for. The measured wing slope (4.14–4.37 px per 100
steps) was 2.2× the predicted 2.012, and focus-model.md § Sweep sizing
reads that as a wrong input. The input was `microns_per_step`, which `rp`
took from the FocusLynx server's ASCOM `StepSize` (1.06985 µm). With
`microns_per_step: 2.1` set on the focuser in `rp.json`, the second night's
derived sweep put its ends at 3.0–3.8× (observation 1).

The shared walk on a guide-coupled train and the `refocus-on-temperature`
rule firing on a temperature change are not covered here: they need
`pier1`'s guider and `session-runner` (*Not covered*).

This is not a ConformU run: no driver's Alpaca behaviour was under test.

## What was tested

| | |
|---|---|
| Commit | [`00b87c77`](https://github.com/rusty-photon/rusty-photon/commit/00b87c77) (`main`), published by the nightly run [41252900567](https://github.com/rusty-photon/rusty-photon/actions/runs/41252900567) as `0.1.0+nightly.202610312331.g00b87c7` |
| Build | The published nightly's suite MSI, SHA256 matched against the release's `SHA256SUMS.txt`, installed in place on 2026-11-01 (`0.1.0.26304`). The `FocusModel` feature was added from the same MSI (`msiexec /i … ADDLOCAL=FocusModel`), leaving the installed features alone |
| Host | `rig2`: Windows 11 Pro (10.0.26200, x64) at a remote observatory, the services under the SCM, TLS and the observatory credential throughout (wired by `doctor --fix`). Pegasus Unity was not running, so `upbv2-driver` switched the UPBv2 hub's outputs and was stopped again afterwards |
| Train | `main`: 530 mm / 106 mm refractor (f/5, no obstruction); DSD FP2 flat panel as the cover; Optec FocusLynx through Optec's own Alpaca server (port 4247); QHY600M `QHY600M-3b1ea54688eb9f9d7` (3.76 µm pixels, 1.463″/px, uncooled) with its 7-slot wheel, L R G B H S O. `qhy-camera` pins the camera's USB port to camera 0 and its wheel to wheel 0 (`usb_devices`), and `rp` pins the camera by its `UniqueID` |
| `rp` | `focus-model` registered as a tool provider as focus-model.md § Registration in `rp` gives it; the wheel added to the train before the camera, with wavelengths for H (656.3), S (672.4) and O (500.7); night 2 only, `focusers[focuslynx].microns_per_step: 2.1` |
| `focus-model` | `trains.main`: 4 s frames, `min_area` 6, `max_area` 5000, `threshold_sigma` 8, one frame per step, `min_fit_points` 5, `min_star_fraction` 0.1, `confirmation_tolerance` 0.25, `max_attempts` 2, `step_size` and `half_width` unset (derived); sweep defaults (`end_ratio` 4, 9 points, 2.5″ seeing) |
| Procedure | The train was powered only once the Sun was below −12° and `rp`'s `get_safety_status` read the roof open; the cover was opened and a 4 s frame checked for stars (515 on night 1, 1338 on night 2) before any sweep; afterwards the cover was closed and every switch put back as found. Night 1 was run unattended by a scheduled session following a written runbook, night 2 by hand from the same scripts. Both on 2026-11-04 (UTC), one roof opening |

## Results

`focus-train-results.json` holds the six `focus_train` results as `rp`
relayed them, curve points included.

### The sweep plan

| | Night 1 (1.06985 µm) | Night 2 (2.1 µm) |
|---|---|---|
| `predicted_slope`, px per 100 steps | 2.012 | 3.949 |
| `cfz_steps` | 62.7 | 32.0 |
| No record: step / half width / points | 42 / 165 / 8 | 21 / 84 / 9 |
| After the second run | 44 / 175 / 8 | 24 / 94 / 8 |
| The hand-tuned block it replaces | 25 / 100 | 25 / 100 |

### The runs

| Night | Run | Filter | Prediction | Grid (step, half width) | Focus | R² | Wing slope | Ends, × best HFR |
|---|---|---|---|---|---|---|---|---|
| 1 | 1 | L | none (no record) | 42, 165, centred on 29737 | 29618 | 0.969 | 4.14 | 1.8 / 9.3 (off-centre) |
| 1 | 2 | L | 29618, moved from 29768 | 55, 217 | 29613 | 0.978 | 4.37 | 11.2 / 8.9 |
| 1 | 3 | R | 29633 (+20), move skipped | 42, 165, centred on 29613 | 29627 | 0.976 | 4.23 | 11.3 / 7.1 |
| 2 | 1 | L | none (record reset) | 21, 84, centred on 29613 | 29557 | 0.975 | 3.65 | 1.4 / 7.0 (off-centre) |
| 2 | 2 | L | 29557, moved from 29707 | 23, 90 | 29553 | 0.973 | 3.21 | 3.3 / 3.0 |
| 2 | 3 | R | 29573 (+20), moved from 29553 | 21, 84 | 29568 | 0.978 | 3.12 | 3.4 / 3.8 |

Every run confirmed at its first attempt. The confirmation frames held
1322–2355 stars, and each run took about two minutes. A run with no
prediction is centred where the focuser stood, which on both nights was
the previous focus, 60–120 steps away by then. So the first run of each
night is off-centre, and its ends say nothing about the width.

### The checks

| Check | Night 1 | Night 2 |
|---|---|---|
| A — the derived sweep | partial: confirms on the derived grid, R² 0.969–0.978 against the hand-tuned block's 0.958 on 2026-09-26; ends at 7–11× | **pass**: ends at 3.0–3.8× on the centred runs |
| B — the predicted start inside the band | **pass**: 29618 predicted, 29613 found (5 steps, inside the CFZ) | **pass**: 29557 predicted, 29553 found (4 steps) |
| C — a hand-entered offset on a filter change | **pass**: wheel to R, `terms.offset` 20, start 29633; the move itself was skipped as within half a CFZ (31 steps) of the focuser | **pass**: wheel to R, start 29573, and moved there (20 > 16). R focused at L + 15 (night 1: + 14) |

## Observations

1. **The FocusLynx server's `StepSize` is not this focuser's travel.**
   `rp` reads a focuser's ASCOM `StepSize` when `microns_per_step` is not
   configured (rp.md § Train optics), and Optec's server reported
   1.06985 µm. The night-1 sweeps measured the defocus growth at 2.2× what
   that value predicts, which implies about 2.26 µm per step. The
   focus-model plan's night-3 numbers had already put the slope near
   4 px per 100 steps. With 2.1 µm configured, the predicted slope (3.949)
   is within 6% of night 1's wide-grid measurements. Night 2's narrower
   grids reached only 3–4× the best HFR, sampled less of the wing's
   straight part, and fitted lower slopes (3.12–3.65).
2. **Focus follows temperature at about 14 steps per °C.** L focus was
   29613 at 14.4 °C (01:18Z) and 29553 at 10.0 °C (05:14Z). The change of
   `microns_per_step` made the record stale, as an optical fact is part of
   its identity, and night 2's first run replaced it, so the record now
   holds three runs at 10 °C. `calibrate_temperature` needs five runs over
   3 °C.
3. **`focus_train` reported `model: "fresh"` on a train's first run**, where
   focus-model.md promises `empty`; the fix is
   [#1702](https://github.com/rusty-photon/rusty-photon/pull/1702).
4. **The record left on rig2:** last good L 29553 and R 29568 at
   10.0–10.1 °C, offsets back to `{L 0}`, the train on L at 29553.

## Not covered

- The **shared walk** (`shared: true`) on a guide-coupled train, and the
  **`refocus-on-temperature`** rule firing `focus_train` on a temperature
  change: rig2 has no guider or `session-runner`. Both are left to
  `pier1`, validated as part of S7, which is what rewires the rule to
  call `focus_train`.
- **`calibrate_temperature`**, and a prediction with a temperature term:
  the record has no coefficient yet (observation 2).
- **`determine_filter_offsets`**: the offset under test was hand-entered.

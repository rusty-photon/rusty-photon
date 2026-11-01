# Star-detection fixtures

Real camera pixels for the star-detection tests. The design doc says why
detection is validated on real frames: `docs/services/rp.md`, `measure_basic`
contract.

## `qhy600m_best_focus_crop.fits`

A 512×512 crop of a real frame, unmodified apart from the crop. It is 16-bit
FITS (`BZERO` 32768) with no other header cards.

- **Source frame:** document `0403b71a-0a15-4a72-ad83-4c4291332daa`. That is
  the lowest-HFR sample of an `auto_focus` sweep on rig2, 2026-09-26 UTC: a
  QHY600M, uncooled, bin 1, 9576×6384, through a 530 mm scope.
- **Crop:** columns 960–1471 and rows 3200–3711 of that frame (0-based,
  row 0 first).
- **Content:** it was chosen as the 512×512 window of the frame with the most
  stars of HFR above 0.5 px.
  - Twelve stars are near best focus, at HFR 0.6–1.4 px.
  - The sensor's hot pixels are spread over the whole crop.
  - At `threshold_sigma 5`, `min_area 4` and `max_area 400`:
    - A detector that smooths before removing hot pixels reports 21 objects.
      Nine of them are hot pixels with HFR under 0.25 px, and the median HFR
      is 0.79 px.
    - The twelve stars' component HFRs (`measure_basic` steps 1–8) have a
      median of 1.04 px. `measure_basic` reports the twelve stars at an
      aperture HFR of 1.14 px, the median of all twelve, since there are
      fewer than twenty.

## `qhy600m_defocused_crop.fits`

A 512×512 crop of a real frame, unmodified apart from the crop, in the same
format as the crop above.

- **Source frame:** document `6e47ad6a-5e8b-4738-b7c7-fbd4533af925`. That is
  the sample 800 steps outside focus of the coarse `auto_focus` sweep on rig2,
  2026-09-08 UTC (issue #1187): the same QHY600M at bin 1, 9576×6388, 4 s.
- **Crop:** columns 3673–4184 and rows 1469–1980 of that frame (0-based,
  row 0 first), centred on one of the frame's three stars.
- **Content:** the star is a blur disc about 105 px across, about 60 ADU above
  a background whose standard deviation is about 9 ADU, so it lies near the
  detection threshold and fragments there.
  - At `threshold_sigma 8`, `min_area 6` and `max_area 5000`:
    - The detector reports 32 components, with a median component HFR of
      1.51 px.
    - `measure_basic` reports one star, at an aperture HFR of 38.2 px.
  - At `max_area` below about 4600 the star's half-flux disc is larger than
    `max_area`, and `measure_basic` reports no star: the spots on the disc,
    whose own apertures settle small, lie inside the aperture the disc's
    fragments were growing into.
  - At `threshold_sigma` 4 to 6 the disc is one component, larger than
    `max_area 5000`, and `measure_basic` reports the same star, at
    38.10–38.14 px: its `max_area` bounds the star's half-flux disc, not
    the component (`detect_stars`, which reports components, drops it).

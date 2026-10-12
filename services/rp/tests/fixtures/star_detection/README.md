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
    - `measure_basic` reports the twelve stars, at a median HFR of 1.04 px.

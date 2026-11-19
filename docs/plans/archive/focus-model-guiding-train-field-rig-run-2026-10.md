# focus-model: a training run on the field rig's guiding train, 2026-10-15

A record, moved whole from `docs/services/focus-model.md` on 2026-11-11:
what a probe of a training run on the field rig's guiding train showed
on 2026-10-15 — the refusal with PHD2 looping, the run with PHD2
`Stopped`, the measured cost of a frame and of a move, and the run that
met the bottom of the Scops OAG's travel. It stood in
[focus-model.md § Training runs on a guiding train](../../services/focus-model.md#training-runs-on-a-guiding-train),
whose lease-size constants it measures.

**Real-hardware validation.** 2026-10-15, on the field rig (Raspberry
Pi 5, aarch64), at `307f0921`: `rp`, `focus-model` and `phd2-guider`
built from the branch, with the guiding train `[zwo-eaf, scops-oag,
guide camera]` ending in the PHD2 camera facade over the rig's
QHY5III715C behind PHD2 2.6.14, and the real EAF, Scops OAG and Falcon.
The rig was indoors, so the room-lit frames had no stars, and the train
block pinned a detection threshold no frame could clear, so the sweep
ended in `not_enough_stars`. Its sweep was configured, 9 points 10 steps
apart, with 1 s exposures and two attempts. A probe that asserted every
outcome passed each check:

- with PHD2 looping, the run was refused at the lease, relaying "PHD2
  is Looping"; neither focuser moved and no sweep lease was left;
- with PHD2 `Stopped`, the run asked for a 2m 36s lease, walked both
  attempts' 18 samples through the facade and failed on its stars. It
  put the OAG back where it started and never moved the EAF, the
  shared focuser the lease held still. PHD2 stayed `Stopped`;
- `phd2-guider` refused every guiding operation the probe sent during
  the run, every 2 s, naming the sweep lease or a frame still exposing,
  and accepted them again once the run had answered;
- the run was recorded with its 18 curve points, and `rp` then granted
  a new lease on the train, so nothing of the old one was left.

The run held its lease for 46.7 s. From `focus-model`'s own call log, a
frame cost 1.40 s beyond its 1 s exposure on average and 1.47 s at
most, the full 3864 × 2192 frame through PHD2, the facade, `rp`'s FITS
write and `measure_stars` included, and a 10-step move cost 0.17 s on
average and 0.41 s at most. Both 2 s constants hold, the frame's with
a third to spare.

The first attempt found the OAG at 0, the bottom of its travel. `rp`'s
config sets no travel bounds for it, so the provider did not clamp its
grid, and the driver refused the first sample at −40. The run failed
and put the OAG back, and the probe started the next run at 200. That
gap was older than the training run and applied to every train
([#1529](https://github.com/rusty-photon/rusty-photon/issues/1529)):
`rp` now reports a focuser's driver travel, `0` to `MaxStep`, as its
bounds where the config sets none, so the provider clamps such a grid
at `0`.

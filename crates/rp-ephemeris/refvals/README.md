# Reference values for `rp-ephemeris`

`gen.py` writes the JSON files here, which pin canonical topocentric
alt/az for nine fixed objects at three sites and four times. They are
committed. The matching test in `tests/reference_values.rs` asserts
that `ErfarsEphemeris` reproduces each `alt_az` entry within `1e-4°`
(≈ 0.36″), and fails when it finds no file.

The JSONs also carry the ICRS RA/Dec used to compute the alt/az —
the test reads those back as inputs for the trait call, so a
provenance audit is possible — but the assertion itself is on
alt/az only. `gen.py` writes alt/az and nothing else, so transit,
rise/set and the trait's own Sun and Moon positions are not
cross-checked against Astropy. Nor are the Sun and the Moon among the
objects: their ICRS place is barycentric, and `alt_az`, built for a
star's catalog place, cannot take it as given. The tolerance is tight
enough that an alt/az regression from a wrapping bug surfaces at once.

Astropy uses ERFA internally, so with the same inputs agreement is
near-perfect: the committed values match ERFA's `atco13` to about
1e-11°. `gen.py` gives Astropy the inputs `ErfarsEphemeris` passes:
the refraction conditions, sea level, and zero ΔUT1 and polar motion
in place of the IERS values Astropy would otherwise apply, which
differ by up to several arcseconds. Disagreement is a wrapping bug,
not an algorithmic one — the test points at the seam, not at the
math.

## Regenerating

```sh
pip install 'astropy>=6' numpy
python3 crates/rp-ephemeris/refvals/gen.py
```

The script is run **by hand**, not in CI. Re-run after upgrading
`erfars` or `astropy`. Commit the diffed JSONs alongside the dep
bump so a reviewer can verify provenance from the diff.

The generator is deterministic for a fixed Astropy version and a
fixed input table; pin the version in the commit message.

## Why not run `gen.py` in CI?

- It would tie test pass/fail to whichever Astropy point release is
  on the runner that day.
- It hides whether the values were actually computed offline against
  the version the workspace was authored against.
- It adds a Python toolchain dependency to the Rust build.

Committed JSONs are the artefact under review. Treat them like
golden test fixtures: changes are explicit.

# Reference values for `rp-ephemeris`

`gen.py` writes the JSON files here, which pin canonical topocentric
alt/az for nine fixed objects, and for the Moon, at three sites and
four times. They are committed. The matching tests in
`tests/reference_values.rs` assert that `ErfarsEphemeris` reproduces
each object's `alt_az` entry within `1e-4°` (≈ 0.36″) and the Moon's
within 10″, and fail when they find no file.

The JSONs also carry the ICRS RA/Dec used to compute the alt/az —
the test reads those back as inputs for the trait call, so a
provenance audit is possible — but the assertion itself is on
alt/az only. `gen.py` writes alt/az and nothing else, so transit,
rise/set, the trait's Sun position and the Moon's RA/Dec are not
cross-checked against Astropy. Nor are the Sun and the Moon among the
objects: their ICRS place is barycentric, and `alt_az`, built for a
star's catalog place, cannot take it as given. Each file holds the
Moon's alt/az in an entry of its own instead, which the test compares
with `moon_position`. The objects' tolerance is tight enough that an
alt/az regression from a wrapping bug surfaces at once.

Astropy uses ERFA internally, so with the same inputs agreement is
near-perfect: the committed values match ERFA's `atco13` to about
1e-11°. `gen.py` gives Astropy the inputs `ErfarsEphemeris` passes:
the refraction conditions, sea level, and zero ΔUT1 and polar motion
in place of the IERS values Astropy would otherwise apply, which
differ by up to several arcseconds. Disagreement is a wrapping bug,
not an algorithmic one — the test points at the seam, not at the
math.

The Moon is the exception. Astropy's "builtin" ephemeris takes the
Moon from ERFA's own `moon98`, which would check the crate against
itself, so `gen.py` takes it from JPL's DE440s, seen from the site
through `get_body`. DE440s is set as the session's ephemeris rather
than passed as `get_body`'s `ephemeris` argument, which covers the
Moon alone: the transform inside `get_body` would take the Earth from
"builtin", and that moves the Moon by about half an arcminute.
`Moon98` is a truncated lunar theory and agrees with DE440s to about
4″ at these times, so the Moon's test allows 10″: room for that, and
far below the Moon's parallax, half a degree or more at every one of
these times.

## Regenerating

```sh
pip install 'astropy>=6' numpy jplephem
python3 crates/rp-ephemeris/refvals/gen.py
```

The first run downloads DE440s (about 32 MB) into Astropy's cache.

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

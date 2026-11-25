#!/usr/bin/env python3
"""Generate reference values for `rp-ephemeris` against Astropy.

Astropy uses ERFA internally — agreement with `ErfarsEphemeris` should
be near-perfect (sub-arcsecond for alt/az). This script writes one JSON
file per (time, site) pair, holding every object's alt/az, under
``crates/rp-ephemeris/refvals/``; the matching test in
``tests/reference_values.rs`` reads those files and asserts the trait's
``alt_az`` matches within 1e-4°. Disagreement is a wrapping bug, not an
algorithmic one.

Run by hand, not in CI. Re-run after upgrading erfars or astropy.

Requirements:

    pip install 'astropy>=6' numpy

Provenance is the commit message + this file: commit the generated
JSONs alongside it, and a reviewer re-runs the script and diffs to
confirm the values in the PR.

Usage::

    python3 crates/rp-ephemeris/refvals/gen.py
"""

from __future__ import annotations

import json
import pathlib
from datetime import datetime, timezone

try:
    from astropy import units as u
    from astropy.coordinates import (
        AltAz,
        EarthLocation,
        SkyCoord,
    )
    from astropy.table import QTable
    from astropy.time import Time
    from astropy.utils import iers
except ImportError as exc:  # pragma: no cover - tooling
    raise SystemExit(
        "astropy is required: pip install 'astropy>=6' numpy"
    ) from exc

REFVALS_DIR = pathlib.Path(__file__).resolve().parent

# Targets: name, ICRS RA (hours), Dec (deg). A deep-sky mix, a polar
# object (Polaris) and southern objects. The Sun and the Moon are not
# among them: their ICRS place is barycentric, which `alt_az`, built for a
# star's catalog place, cannot take as given.
TARGETS = {
    "polaris": (2.5301944, 89.2641111),
    "m31": (0.71223, 41.26878),
    "m42": (5.58809, -5.39119),
    "m81": (9.92577, 69.06529),
    "sirius": (6.7525, -16.71611),
    "vega": (18.61567, 38.78367),
    "antares": (16.49013, -26.43200),
    "ngc891": (2.37500, 42.34917),
    "ic1396": (21.65083, 57.50833),
}

# Sites: name, lat, lon. Mid-northern, mid-southern, equatorial.
SITES = {
    "seattle": (47.6062, -122.3321),
    "santiago": (-33.4489, -70.6693),
    "quito": (-0.1807, -78.4678),
}

# Times: solstices and an equinox (UTC). 2026 epoch.
TIMES = {
    "vernal_2026": "2026-03-20T14:46:00Z",
    "summer_2026": "2026-06-21T08:24:00Z",
    "autumnal_2026": "2026-09-22T23:05:00Z",
    "winter_2026": "2026-12-21T19:50:00Z",
}


# Atmospheric conditions must match the constants hard-coded in
# `crates/rp-ephemeris/src/erfars_impl.rs::alt_az_at`. Astropy's
# default is no refraction (zero pressure); without these the
# generated reference values would diverge from `ErfarsEphemeris`
# by tens of arcseconds near the horizon.
PRESSURE = 1013.25 * u.hPa  # millibar (numerically identical to hPa)
TEMPERATURE = 10.0 * u.deg_C
RELATIVE_HUMIDITY = 0.5
OBSWL = 0.55 * u.micron


def _serialise_alt_az(coord: SkyCoord, location: EarthLocation, when: Time):
    altaz = coord.transform_to(
        AltAz(
            obstime=when,
            location=location,
            pressure=PRESSURE,
            temperature=TEMPERATURE,
            relative_humidity=RELATIVE_HUMIDITY,
            obswl=OBSWL,
        )
    )
    return {
        "altitude_degrees": float(altaz.alt.deg),
        "azimuth_degrees": float(altaz.az.deg),
    }


def _zero_earth_orientation() -> None:
    # `ErfarsEphemeris` passes dUT1 = 0 and no polar motion to `Atco13`;
    # Astropy otherwise applies the IERS values, which differ from those by
    # up to several arcseconds in alt/az. A table of zeros over every date
    # here gives Astropy the same inputs.
    table = QTable(
        {
            "MJD": [40000.0, 80000.0] * u.d,
            "UT1_UTC": [0.0, 0.0] * u.s,
            "PM_x": [0.0, 0.0] * u.arcsec,
            "PM_y": [0.0, 0.0] * u.arcsec,
        }
    )
    iers.earth_orientation_table.set(iers.IERS(table))


def main() -> None:
    REFVALS_DIR.mkdir(parents=True, exist_ok=True)
    _zero_earth_orientation()

    for time_label, iso in TIMES.items():
        when = Time(datetime.fromisoformat(iso.replace("Z", "+00:00")))
        for site_label, (lat, lon) in SITES.items():
            location = EarthLocation(lat=lat * u.deg, lon=lon * u.deg, height=0 * u.m)

            entries = {}
            for target_label, (ra_h, dec_d) in TARGETS.items():
                coord = SkyCoord(
                    ra=ra_h * 15 * u.deg, dec=dec_d * u.deg, frame="icrs"
                )
                entries[target_label] = {
                    "ra_hours": ra_h,
                    "dec_degrees": dec_d,
                    "alt_az": _serialise_alt_az(coord, location, when),
                }

            output = {
                "site": {
                    "label": site_label,
                    "latitude_degrees": lat,
                    "longitude_degrees": lon,
                },
                "time_utc": iso,
                "time_label": time_label,
                "targets": entries,
            }

            out = REFVALS_DIR / f"{time_label}__{site_label}.json"
            out.write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
            print(f"wrote {out.relative_to(REFVALS_DIR.parent)}")


if __name__ == "__main__":
    main()

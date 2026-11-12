#!/usr/bin/env python3
"""Download the HD-depth star layer for `catalog.bin`.

Source: VizieR IV/25 `tyc2_hd` (Fabricius+ 2002), the Tycho-2/HD
cross-index — every HD/HDE/HDEC designation with a Tycho-2 counterpart
(~353k distinct HDs). Positions are the VizieR-computed J2000 `_RA`/`_DE`
(Tycho-2-derived, ICRS) — **not** the original 1900-epoch HD positions.
Magnitudes come from a server-side join against Tycho-2 (I/259) and are
converted to Johnson V via V ≈ VT − 0.090·(BT − VT) (Tycho-2
documentation approximation; VT alone when BT is missing).

The output is an *uncommitted* intermediate (`scripts/build/stars_iv25.csv`,
gitignored) consumed by `pack_catalog.py`; only the packed section inside
`src/data/catalog.bin` is committed. Rows are sorted by HD number so
re-runs are diffable.

Usage::

    python3 crates/rp-catalog/scripts/fetch_star_layer.py
"""

from __future__ import annotations

import csv
import sys
import urllib.parse
import urllib.request
from io import StringIO
from pathlib import Path

VIZIER_TAP = "https://tapvizier.cds.unistra.fr/TAPVizieR/tap/sync"
OUT = Path(__file__).resolve().parent / "build" / "stars_iv25.csv"

QUERY = (
    'SELECT h.HD AS hd, h."_RA" AS ra, h."_DE" AS dec, '
    "t.BTmag AS bt, t.VTmag AS vt "
    'FROM "IV/25/tyc2_hd" AS h '
    'LEFT JOIN "I/259/tyc2" AS t '
    "ON h.TYC1 = t.TYC1 AND h.TYC2 = t.TYC2 AND h.TYC3 = t.TYC3"
)


def main() -> None:
    data = urllib.parse.urlencode(
        {
            "REQUEST": "doQuery",
            "LANG": "ADQL",
            "FORMAT": "csv",
            "MAXREC": "400000",
            "QUERY": QUERY,
        }
    ).encode()
    print("querying VizieR (IV/25 x I/259 join, ~354k rows) ...")
    req = urllib.request.Request(VIZIER_TAP, data=data)
    with urllib.request.urlopen(req, timeout=1800) as resp:
        body = resp.read().decode()

    stars: dict[int, tuple[float, float, float | None]] = {}
    for r in csv.DictReader(StringIO(body)):
        if not r["hd"] or not r["ra"] or not r["dec"]:
            continue
        hd = int(r["hd"])
        vt = float(r["vt"]) if r["vt"] else None
        bt = float(r["bt"]) if r["bt"] else None
        if vt is not None:
            v = vt - 0.090 * (bt - vt) if bt is not None else vt
        else:
            v = None
        # A handful of HDs appear under two Tycho entries (components);
        # keep the brighter one — it is the star the designation means.
        if hd in stars:
            prev_v = stars[hd][2]
            if v is None or (prev_v is not None and prev_v <= v):
                continue
        stars[hd] = (float(r["ra"]), float(r["dec"]), v)

    OUT.parent.mkdir(parents=True, exist_ok=True)
    with OUT.open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["hd", "ra_degrees", "dec_degrees", "vmag"])
        for hd in sorted(stars):
            ra, dec, v = stars[hd]
            w.writerow([hd, f"{ra:.5f}", f"{dec:.5f}", f"{v:.2f}" if v is not None else ""])
    print(f"stars_iv25.csv: {len(stars)} rows")
    if not 340_000 <= len(stars) <= 365_000:
        print(f"FAILED sanity rail: {len(stars)} stars", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()

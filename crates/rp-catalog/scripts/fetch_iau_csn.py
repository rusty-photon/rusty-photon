#!/usr/bin/env python3
"""Convert the IAU Catalog of Star Names (WGSN) into the committed
`named_stars.csv` (`name,hd,magnitude`): the ~450 stars whose proper
name is the canonical catalog entry, so that a tap-anchor import on
Vega arrives as "Vega", not "HD 172167". The CSN magnitude
rides along because Tycho-2 lacks VT for the very brightest stars, so
the packer backfills named rows from it.

Source: https://www.pas.rochester.edu/~emamajek/WGSN/IAU-CSN.txt
(IAU data, Creative Commons Attribution — see ../src/data/LICENSE-DATA).
Names without an HD identifier (faint exoplanet hosts, a few components)
are skipped with a note: they are absent from the IV/25 star layer, so
there is no row for them to name.

Usage::

    python3 crates/rp-catalog/scripts/fetch_iau_csn.py [IAU-CSN.txt]

With no argument the file is downloaded. Output is sorted by name so
re-runs produce stable diffs.
"""

from __future__ import annotations

import csv
import re
import sys
import urllib.request
from pathlib import Path

CSN_URL = "https://www.pas.rochester.edu/~emamajek/WGSN/IAU-CSN.txt"
OUT = Path(__file__).resolve().parent.parent / "src" / "data" / "named_stars.csv"
DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")


def main() -> None:
    if len(sys.argv) > 1:
        text = Path(sys.argv[1]).read_text(encoding="utf-8")
    else:
        with urllib.request.urlopen(CSN_URL, timeout=60) as resp:
            text = resp.read().decode("utf-8")

    rows: dict[str, tuple[str, str]] = {}
    hd_seen: dict[str, str] = {}
    skipped = 0
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith(("#", "$")):
            continue
        # Column (1) Name/ASCII is fixed-width (18 chars); the numeric
        # tail is located relative to the date column (15) because the
        # trailing notes column is optional.
        name = line[:18].strip()
        tokens = line.split()
        date_idx = next((i for i, t in enumerate(tokens) if DATE_RE.match(t)), None)
        if not name or date_idx is None:
            continue
        hd = tokens[date_idx - 3]
        # 999999 is the CSN's own no-HD placeholder (Proxima Centauri);
        # real HD/HDE/HDEC numbers end at 359083.
        if not hd.isdigit() or int(hd) > 359_083:
            skipped += 1
            continue
        if hd in hd_seen:
            print(f"  ! {name}: HD {hd} already named {hd_seen[hd]}, keeping first")
            continue
        try:
            mag = f"{float(tokens[date_idx - 6]):.2f}"
        except ValueError:
            mag = ""
        hd_seen[hd] = name
        rows[name] = (hd, mag)

    with OUT.open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["name", "hd", "magnitude"])
        w.writerows((n, hd, mag) for n, (hd, mag) in sorted(rows.items()))
    print(f"named_stars.csv: {len(rows)} rows ({skipped} skipped without HD)")
    if not 400 <= len(rows) <= 550:
        print(f"FAILED sanity rail: {len(rows)} named stars", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()

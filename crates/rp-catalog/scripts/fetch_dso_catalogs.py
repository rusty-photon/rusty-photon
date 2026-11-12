#!/usr/bin/env python3
"""Fetch the astrophoto DSO catalogs into the per-catalog
CSVs committed under `crates/rp-catalog/src/data/`.

Sources (see ../src/data/LICENSE-DATA):

- SIMBAD TAP (https://simbad.cds.unistra.fr/simbad/sim-tap) for every
  identifier-list catalog: Sharpless (SIMBAD's curated positions, not the
  raw VII/20 coordinates),
  Barnard, LDN, LBN, RCW, Gum, Cederblad, Abell planetaries, Arp,
  Hickson, Collinder, Melotte, Stock, Trumpler. Common-name aliases
  come from SIMBAD `NAME` cross-identifiers.
- VizieR TAP (https://tapvizier.cds.unistra.fr/TAPVizieR/tap) table
  VII/21 for the vdB reflection nebulae — SIMBAD's `Cl VDB` coverage
  is only ~1/3 of the catalog.

SIMBAD identifiers are fixed-width space-padded ("SH  2-117",
"Cl Melotte   20") and cluster *members* share the cluster prefix with
a trailing member number ("Cl Melotte   20     1"), so every pattern
match is whitespace-normalized and re-filtered against an anchored
regex before use.

Output CSVs use the existing embedded schema
(`name,type,ra_hours,dec_degrees,magnitude,size_arcmin`) plus
`aliases_simbad.csv` (`alias,canonical_name`). Rows are sorted by
catalog number so re-runs produce stable diffs.

Usage::

    python3 crates/rp-catalog/scripts/fetch_dso_catalogs.py

Re-run after upstream data improves; commit the diff so provenance
lives in git history.
"""

from __future__ import annotations

import csv
import re
import sys
import urllib.parse
import urllib.request
from io import StringIO
from pathlib import Path

SIMBAD_TAP = "https://simbad.cds.unistra.fr/simbad/sim-tap/sync"
VIZIER_TAP = "https://tapvizier.cds.unistra.fr/TAPVizieR/tap/sync"

OUT_DIR = Path(__file__).resolve().parent.parent / "src" / "data"

# (csv stem, SIMBAD LIKE pattern, anchored id regex after whitespace
#  squeeze, canonical display prefix, allowed type codes | None = free,
#  fallback type code, expected count range). Expected ranges are sanity
#  rails against silently-broken upstream queries, not exact pins —
#  SIMBAD curation moves, and SIMBAD's identifier coverage of the
#  Collinder/Melotte/Gum lists is genuinely partial (the missing numbers
#  are mostly objects whose primary designation is NGC/IC, which already
#  have rows of their own).
#
# The allowed set constrains SIMBAD's otype to the designation's own
# class: a Sharpless number denotes an HII region even when SIMBAD's
# merged object is classed as the embedded cluster (SH 2-117 = NGC 7000
# carries otype OpC), and a Barnard/LDN number denotes the dark cloud
# no matter what else sits there.
NEBULAR = {"HII", "SNR", "PN", "EmN", "Neb", "RfN"}
GALACTIC = {"G", "GPair", "GTrpl", "GGroup"}
SIMBAD_CATALOGS = [
    ("sh2", "SH  2-%", r"^SH 2-(\d+)$", "Sh2-{n}", NEBULAR, "HII", (300, 330)),
    ("barnard", "Barnard %", r"^Barnard (\d+)$", "B {n}", set(), "DrkN", (330, 360)),
    ("ldn", "LDN %", r"^LDN (\d+)$", "LDN {n}", set(), "DrkN", (1700, 1850)),
    ("lbn", "LBN %", r"^LBN (\d+)$", "LBN {n}", NEBULAR, "Neb", (1050, 1200)),
    ("rcw", "RCW %", r"^RCW (\d+)$", "RCW {n}", NEBULAR, "HII", (160, 200)),
    ("gum", "GUM %", r"^GUM (\d+)$", "Gum {n}", NEBULAR, "HII", (60, 100)),
    ("ced", "Ced %", r"^Ced (\d+[a-e]?)$", "Ced {n}", NEBULAR, "RfN", (170, 230)),
    ("abell", "PN A66 %", r"^PN A66 (\d+)$", "Abell {n}", set(), "PN", (80, 90)),
    ("arp", "APG %", r"^APG (\d+)$", "Arp {n}", GALACTIC, "G", (320, 345)),
    ("hcg", "HCG %", r"^HCG (\d+)$", "HCG {n}", set(), "GGroup", (95, 105)),
    ("collinder", "Cl Collinder %", r"^Cl Collinder (\d+)$", "Cr {n}", set(), "OCl", (150, 480)),
    ("melotte", "Cl Melotte %", r"^Cl Melotte (\d+)$", "Mel {n}", set(), "OCl", (60, 250)),
    ("stock", "Cl Stock %", r"^Cl Stock (\d+)$", "Stock {n}", set(), "OCl", (20, 45)),
    ("trumpler", "Cl Trumpler %", r"^Cl Trumpler (\d+)$", "Tr {n}", set(), "OCl", (30, 45)),
]

# SIMBAD otype -> OpenNGC-style code used across the embedded CSVs
# (exact spellings from the existing ngc.csv/ic.csv vocabulary). The
# result must land in the catalog's allowed set or it falls back.
OTYPE_MAP = {
    "HII": "HII",
    "DNe": "DrkN",
    "RNe": "RfN",
    "SNR": "SNR",
    "PN": "PN",
    "OpC": "OCl",
    "Cl*": "OCl",
    "GlC": "GCl",
    "As*": "*Ass",
    "G": "G",
    "IG": "GPair",
    "PaG": "GPair",
    "GrG": "GGroup",
    "CGG": "GGroup",
    "ClG": "GGroup",
    "EmG": "G",
    "SBG": "G",
    "GiG": "G",
    "GiP": "G",
    "rG": "G",
    "LSB": "G",
    "AGN": "G",
    "Sy1": "G",
    "Sy2": "G",
    "SyG": "G",
    "LIN": "G",
    "Neb": "Neb",
    "GNe": "Neb",
    "Cld": "Neb",
    "ISM": "Neb",
    "EmO": "EmN",
    "SFR": "HII",
}


def tap_query(url: str, adql: str) -> list[dict[str, str]]:
    data = urllib.parse.urlencode(
        {
            "REQUEST": "doQuery",
            "LANG": "ADQL",
            "FORMAT": "csv",
            "MAXREC": "200000",
            "QUERY": adql,
        }
    ).encode()
    req = urllib.request.Request(url, data=data)
    with urllib.request.urlopen(req, timeout=300) as resp:
        body = resp.read().decode()
    return list(csv.DictReader(StringIO(body)))


def squeeze(s: str) -> str:
    return re.sub(r"\s+", " ", s.strip())


def fmt(value: str, digits: int) -> str:
    if value in ("", None):
        return ""
    return f"{float(value):.{digits}f}"


def natural_key(name: str) -> tuple:
    return tuple(int(t) if t.isdigit() else t for t in re.split(r"(\d+)", name))


def fetch_simbad_catalog(
    pattern: str, id_re: str, display: str, allowed: set, fallback_type: str
):
    """One catalog: rows keyed by canonical display name, and the set of
    matched SIMBAD oids for the later alias pass. An empty `allowed` set
    forces the fallback type for every row."""
    adql = (
        "SELECT i.id AS cid, b.oid AS oid, b.ra AS ra, b.dec AS dec, "
        "b.otype AS otype, b.galdim_majaxis AS majaxis, f.V AS vmag "
        "FROM ident i JOIN basic b ON i.oidref = b.oid "
        "LEFT JOIN allfluxes f ON f.oidref = b.oid "
        f"WHERE i.id LIKE '{pattern}'"
    )
    rx = re.compile(id_re)
    rows: dict[str, list[str]] = {}
    oid_to_name: dict[str, str] = {}
    for r in tap_query(SIMBAD_TAP, adql):
        m = rx.match(squeeze(r["cid"]))
        if not m:
            continue
        if r["ra"] in ("", None) or r["dec"] in ("", None):
            print(f"  ! {squeeze(r['cid'])}: no position in SIMBAD, skipped")
            continue
        name = display.format(n=m.group(1))
        ra_hours = float(r["ra"]) / 15.0
        otype = OTYPE_MAP.get((r["otype"] or "").strip(), fallback_type)
        if otype not in allowed:
            otype = fallback_type
        row = [
            name,
            otype,
            f"{ra_hours:.6f}",
            f"{float(r['dec']):.6f}",
            fmt(r["vmag"], 2),
            fmt(r["majaxis"], 2),
        ]
        # A handful of ids resolve to the same object twice (rare SIMBAD
        # duplicates); keep the first, deterministically by number order
        # later via the final sort.
        if name not in rows:
            rows[name] = row
            oid_to_name[r["oid"]] = name
    return rows, oid_to_name


def fetch_simbad_aliases(pattern: str, id_re: str, oid_to_name: dict[str, str]):
    """NAME cross-ids for the objects matched by `pattern`."""
    adql = (
        "SELECT i1.id AS cid, i1.oidref AS oid, i2.id AS alias "
        "FROM ident i1 JOIN ident i2 ON i1.oidref = i2.oidref "
        f"WHERE i1.id LIKE '{pattern}' AND i2.id LIKE 'NAME %'"
    )
    rx = re.compile(id_re)
    pairs: list[tuple[str, str]] = []
    for r in tap_query(SIMBAD_TAP, adql):
        if not rx.match(squeeze(r["cid"])):
            continue
        canonical = oid_to_name.get(r["oid"])
        if canonical is None:
            continue
        alias = squeeze(r["alias"])[len("NAME "):]
        if alias and alias.lower() != canonical.lower():
            pairs.append((alias, canonical))
    return pairs


def fetch_vdb():
    """vdB reflection nebulae from VizieR VII/21 (van den Bergh 1966).
    `_RA`/`_DE` are the VizieR-computed J2000 positions; size is twice
    the larger of the blue/red maximum radii."""
    adql = (
        'SELECT "VdB" AS n, "Vmag" AS vmag, "BRadMax" AS brad, '
        '"RRadMax" AS rrad, "_RA" AS ra, "_DE" AS dec FROM "VII/21/catalog"'
    )
    rows: dict[str, list[str]] = {}
    for r in tap_query(VIZIER_TAP, adql):
        if r["ra"] in ("", None) or r["dec"] in ("", None):
            print(f"  ! vdB {r['n']}: no position, skipped")
            continue
        name = f"vdB {int(r['n'])}"
        radii = [float(x) for x in (r["brad"], r["rrad"]) if x not in ("", None)]
        size = f"{2 * max(radii):.2f}" if radii else ""
        rows[name] = [
            name,
            "RfN",
            f"{float(r['ra']) / 15.0:.6f}",
            f"{float(r['dec']):.6f}",
            fmt(r["vmag"], 2),
            size,
        ]
    return rows


HEADER = ["name", "type", "ra_hours", "dec_degrees", "magnitude", "size_arcmin"]


def write_catalog(stem: str, rows: dict[str, list[str]]) -> None:
    ordered = sorted(rows.values(), key=lambda r: natural_key(r[0]))
    with (OUT_DIR / f"{stem}.csv").open("w", newline="") as f:
        w = csv.writer(f, quoting=csv.QUOTE_MINIMAL)
        w.writerow(HEADER)
        w.writerows(ordered)


def main() -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    all_aliases: list[tuple[str, str]] = []
    failures = []

    for stem, pattern, id_re, display, allowed, fallback, (lo, hi) in SIMBAD_CATALOGS:
        print(f"fetching {stem} ...")
        rows, oid_to_name = fetch_simbad_catalog(pattern, id_re, display, allowed, fallback)
        if not lo <= len(rows) <= hi:
            failures.append(f"{stem}: {len(rows)} rows, expected {lo}..{hi}")
        write_catalog(stem, rows)
        aliases = fetch_simbad_aliases(pattern, id_re, oid_to_name)
        all_aliases.extend(aliases)
        print(f"  {len(rows)} rows, {len(aliases)} common-name aliases")

    print("fetching vdb (VizieR VII/21) ...")
    vdb = fetch_vdb()
    if not 150 <= len(vdb) <= 165:
        failures.append(f"vdb: {len(vdb)} rows, expected 150..165")
    write_catalog("vdb", vdb)
    print(f"  {len(vdb)} rows")

    all_aliases.sort()
    with (OUT_DIR / "aliases_simbad.csv").open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["alias", "canonical_name"])
        w.writerows(all_aliases)
    print(f"aliases_simbad.csv: {len(all_aliases)} rows")

    if failures:
        print("FAILED sanity rails:\n  " + "\n  ".join(failures), file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()

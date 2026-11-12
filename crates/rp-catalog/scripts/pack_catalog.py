#!/usr/bin/env python3
"""Pack the committed catalog CSVs plus the fetched star layer into
`src/data/catalog.bin` — the single embedded artifact `rp-catalog`
reads zero-copy at runtime (design: `docs/crates/rp-catalog.md`).

Inputs (all relative to `src/data/` unless noted):

- DSO catalogs: messier/ngc/ic (OpenNGC) + sh2/abell/vdb/rcw/gum/ced/
  barnard/ldn/lbn/arp/hcg/collinder/melotte/stock/trumpler (SIMBAD /
  VizieR; see fetch_dso_catalogs.py). File order below fixes the
  catalog rank used to break exact separation ties in `nearest()`.
- `aliases.csv` (OpenNGC common names), then `aliases_curated.csv`
  (hand-maintained names missing from both machine sources, e.g.
  "Tulip Nebula" → Sh2-101), then `aliases_simbad.csv` — processed in
  that order; canonical names beat aliases, first alias wins on
  ambiguity.
- `named_stars.csv` — IAU CSN proper names; these become canonical
  star entries ("Vega") with a display override on their HD row.
- `scripts/build/stars_iv25.csv` (uncommitted; fetch_star_layer.py).

Binary format `RPCAT001`, little-endian throughout, sections tightly
packed in this order (counts from the header make every offset
computable):

    magic     8s  = b"RPCAT001"
    header    6 x u32: dso_count, star_count, key_count,
                        named_count, type_count, pool_len
    dso_dec   i32 x dso_count   declination, mas (sorted ascending)
    dso_ra    u32 x dso_count   right ascension, mas
    dso_mag   i16 x dso_count   centi-magnitudes, i16::MIN = none
    dso_size  u16 x dso_count   deci-arcmin major axis, 0 = none
    dso_type  u8  x dso_count   index into the type table
    dso_cat   u8  x dso_count   catalog rank (tie-break order)
    dso_name  u32 x dso_count   pool offset of the display name
    star_dec  i32 x star_count  mas (sorted ascending)
    star_ra   u32 x star_count  mas
    star_hd   u32 x star_count  HD designation number
    star_mag  i16 x star_count  centi-magnitudes, i16::MIN = none
    keys      (u32 pool_off, u32 row_ref) x key_count
              sorted by normalized key bytes; row_ref bit 31 set =
              star row index, clear = DSO row index
    named     (u32 hd, u32 pool_off) x named_count, sorted by hd
    types     u32 pool_off x type_count
    pool      u8 length-prefixed byte strings

`normalize()` below MUST stay byte-identical to the Rust
`rp_catalog::normalize`. The `embedded_matches_committed_*` tests
resolve the spellings the committed CSVs hold through the Rust function
— a catalog or star name to its own row, an alias only to some row — and
check nothing else, so a change to either is made to both.

Usage::

    python3 crates/rp-catalog/scripts/pack_catalog.py
"""

from __future__ import annotations

import csv
import struct
import sys
from pathlib import Path

DATA_DIR = Path(__file__).resolve().parent.parent / "src" / "data"
STARS_CSV = Path(__file__).resolve().parent / "build" / "stars_iv25.csv"
OUT = DATA_DIR / "catalog.bin"

MAG_NONE = -(2**15)
STAR_BIT = 0x8000_0000

# (file stem, rank). Rank breaks exact separation ties in nearest():
# household designations first. Order here is the packing order, so
# canonical-name collisions resolve the same way every run.
CATALOGS = [
    ("messier", 0),
    ("ngc", 1),
    ("ic", 2),
    ("sh2", 3),
    ("abell", 4),
    ("vdb", 5),
    ("rcw", 6),
    ("gum", 7),
    ("ced", 8),
    ("barnard", 9),
    ("ldn", 10),
    ("lbn", 11),
    ("arp", 12),
    ("hcg", 13),
    ("collinder", 14),
    ("melotte", 15),
    ("stock", 16),
    ("trumpler", 17),
]

# Longest first where one is a prefix of another. Applied once, only
# when the remainder starts with a digit (or is empty), after
# whitespace/dash stripping and ASCII lowering.
PREFIX_REWRITES = [
    ("sharpless2", "sh2"),
    ("sharpless", "sh2"),
    ("messier", "m"),
    ("barnard", "b"),
    ("collinder", "cr"),
    ("melotte", "mel"),
    ("trumpler", "tr"),
    ("hde", "hd"),
]


def normalize(name: str) -> str:
    """Byte-identical mirror of the Rust `rp_catalog::normalize`."""
    buf = "".join(
        chr(ord(c) + 32) if "A" <= c <= "Z" else c
        for c in name
        if not c.isspace() and c != "-"
    )
    for prefix, replacement in PREFIX_REWRITES:
        if buf.startswith(prefix):
            rest = buf[len(prefix):]
            # ASCII digits only — Rust's is_ascii_digit, not unicode.
            if not rest or rest[0] in "0123456789":
                return replacement + rest
            break
    return buf


def mas_ra_from_hours(h: float) -> int:
    v = round(h * 15.0 * 3_600_000)
    return v % 1_296_000_000


def mas_ra_from_degrees(d: float) -> int:
    return round(d * 3_600_000) % 1_296_000_000


def mas_dec(d: float) -> int:
    v = round(d * 3_600_000)
    if not -324_000_000 <= v <= 324_000_000:
        raise ValueError(f"declination out of range: {d}")
    return v


def centimag(s: str) -> int:
    if not s.strip():
        return MAG_NONE
    return round(float(s) * 100)


def deciarcmin(s: str, name: str) -> int:
    if not s.strip():
        return 0
    v = round(float(s) * 10)
    if v > 0xFFFF:
        print(f"  ! {name}: size {s} arcmin clamped to u16 range")
        return 0xFFFF
    return max(v, 1)


class Pool:
    def __init__(self):
        self.buf = bytearray()
        self.offsets: dict[str, int] = {}

    def add(self, s: str) -> int:
        if s in self.offsets:
            return self.offsets[s]
        raw = s.encode("utf-8")
        if len(raw) > 255:
            raise ValueError(f"pool string too long: {s!r}")
        off = len(self.buf)
        self.buf.append(len(raw))
        self.buf.extend(raw)
        self.offsets[s] = off
        return off


def main() -> None:
    pool = Pool()
    types: list[str] = []
    type_idx: dict[str, int] = {}

    # --- DSO rows -------------------------------------------------------
    dsos = []  # (dec_mas, ra_mas, mag, size, type_i, cat, name)
    canonical: dict[str, tuple[str, int]] = {}  # key -> (display, provisional idx)
    for stem, rank in CATALOGS:
        path = DATA_DIR / f"{stem}.csv"
        with path.open(newline="") as f:
            for r in csv.DictReader(f):
                name = r["name"]
                key = normalize(name)
                if key in canonical:
                    raise SystemExit(
                        f"canonical collision: {name!r} vs {canonical[key][0]!r}"
                    )
                t = r["type"]
                if t not in type_idx:
                    type_idx[t] = len(types)
                    types.append(t)
                dsos.append(
                    (
                        mas_dec(float(r["dec_degrees"])),
                        mas_ra_from_hours(float(r["ra_hours"])),
                        centimag(r["magnitude"]),
                        deciarcmin(r["size_arcmin"], name),
                        type_idx[t],
                        rank,
                        name,
                    )
                )
                canonical[key] = (name, len(dsos) - 1)
    if len(types) > 255:
        raise SystemExit(f"{len(types)} type codes exceed u8 table")

    # Sort by dec for the nearest() band search; the permutation is
    # deterministic so canonical indices are remapped after sorting.
    order = sorted(range(len(dsos)), key=lambda i: (dsos[i][0], dsos[i][1], dsos[i][5], dsos[i][6]))
    remap = {old: new for new, old in enumerate(order)}
    dsos = [dsos[i] for i in order]
    keys: dict[str, int] = {k: remap[idx] for k, (_, idx) in canonical.items()}

    # --- star rows ------------------------------------------------------
    named: dict[int, tuple[str, int]] = {}  # hd -> (proper name, centimag)
    with (DATA_DIR / "named_stars.csv").open(newline="") as f:
        for r in csv.DictReader(f):
            named[int(r["hd"])] = (r["name"], centimag(r["magnitude"]))

    stars = []  # (dec_mas, ra_mas, hd, mag)
    if not STARS_CSV.exists():
        raise SystemExit(
            f"{STARS_CSV} missing — run fetch_star_layer.py first"
        )
    with STARS_CSV.open(newline="") as f:
        for r in csv.DictReader(f):
            hd = int(r["hd"])
            mag = centimag(r["vmag"])
            if mag == MAG_NONE and hd in named:
                mag = named[hd][1]
            stars.append(
                (mas_dec(float(r["dec_degrees"])), mas_ra_from_degrees(float(r["ra_degrees"])), hd, mag)
            )
    stars.sort(key=lambda s: (s[0], s[1], s[2]))
    hd_to_row = {s[2]: i for i, s in enumerate(stars)}

    named_rows = []  # (hd, name) for stars actually present
    for hd, (name, _) in sorted(named.items()):
        row = hd_to_row.get(hd)
        if row is None:
            print(f"  ! named star {name}: HD {hd} not in star layer, skipped")
            continue
        key = normalize(name)
        if key in keys:
            print(f"  ! named star {name}: key collides with DSO {key!r}, skipped")
            continue
        keys[key] = STAR_BIT | row
        named_rows.append((hd, name))

    # --- aliases --------------------------------------------------------
    ambiguous = 0
    for fname in ("aliases.csv", "aliases_curated.csv", "aliases_simbad.csv"):
        with (DATA_DIR / fname).open(newline="") as f:
            for r in csv.DictReader(f):
                akey = normalize(r["alias"])
                ckey = normalize(r["canonical_name"])
                target = keys.get(ckey)
                if target is None or target & STAR_BIT:
                    print(f"  ! alias {r['alias']!r}: unknown canonical {r['canonical_name']!r}, skipped")
                    continue
                if akey in keys:
                    if keys[akey] != target:
                        ambiguous += 1
                    continue
                keys[akey] = target

    # --- assemble -------------------------------------------------------
    out = bytearray()
    out += struct.pack("<8s", b"RPCAT001")
    key_list = sorted(keys.items())
    header_pos = len(out)
    out += struct.pack(
        "<6I", len(dsos), len(stars), len(key_list), len(named_rows), len(types), 0
    )
    for field, fmtc in ((0, "<i"), (1, "<I"), (2, "<h"), (3, "<H"), (4, "<B"), (5, "<B")):
        for d in dsos:
            out += struct.pack(fmtc, d[field])
    for d in dsos:
        out += struct.pack("<I", pool.add(d[6]))
    for field, fmtc in ((0, "<i"), (1, "<I"), (2, "<I"), (3, "<h")):
        for s in stars:
            out += struct.pack(fmtc, s[field])
    for k, ref in key_list:
        out += struct.pack("<II", pool.add(k), ref)
    for hd, name in named_rows:
        out += struct.pack("<II", hd, pool.add(name))
    for t in types:
        out += struct.pack("<I", pool.add(t))
    out += pool.buf
    struct.pack_into(
        "<6I",
        out,
        header_pos,
        len(dsos),
        len(stars),
        len(key_list),
        len(named_rows),
        len(types),
        len(pool.buf),
    )

    OUT.write_bytes(out)
    print(
        f"catalog.bin: {len(out):,} bytes — {len(dsos):,} DSOs, "
        f"{len(stars):,} stars, {len(key_list):,} keys "
        f"({ambiguous} ambiguous aliases kept first), "
        f"{len(named_rows)} named stars, {len(types)} type codes"
    )


if __name__ == "__main__":
    main()

# `rp-catalog` Crate Design

An embedded sky catalog that answers two questions without a network: *what
is at this name* (`Catalog::resolve`) and *what is the nearest named thing to
this coordinate* (`Catalog::nearest`). The first backs `rp`'s
`resolve_target` tool and the `catalog_ref` form of `add_target`; the second
names a target that arrives as bare coordinates, which is how a planetarium
import arrives
([`rp.md` § Import form](../services/rp.md#import-form-source)).

This is a workspace library, not a service. It does no I/O at run time: the
data is one binary blob compiled into the crate. Typing a Messier number and
getting coordinates is too central to depend on a plugin or a connection, and
a remote dark site may have neither.

## Contents

One logical catalog spans two entry classes (`ObjectClass`):

| Class | Rows | What | Source |
|-------|------|------|--------|
| `DeepSky` | ~19k | Messier, NGC, IC | [OpenNGC](https://github.com/mattiaverga/OpenNGC) |
| | | Sharpless, Abell planetaries, RCW, Gum, Cederblad, Barnard, LDN, LBN, Arp, Hickson, Collinder, Melotte, Stock, Trumpler | SIMBAD, by TAP |
| | | van den Bergh reflection nebulae | VizieR VII/21 |
| `Star` | ~354k | Every HD, HDE and HDEC designation | The Tycho-2/HD cross-index, VizieR IV/25 |

Three choices of source are deliberate:

- **Sharpless positions come from SIMBAD, not from the 1959 list** (VizieR
  VII/20), whose coordinates are unreliable for some entries.
- **HD positions come from the Tycho-2 cross-index, not from the HD catalogue
  itself**, which is epoch B1900 at photographic-plate quality. The positions
  are J2000; the magnitude is a Johnson V approximated from Tycho-2
  (`V ≈ VT − 0.090·(BT − VT)`).
- **The star layer stops at HD.** Full Tycho-2 is 2.5 million rows, and the
  stars it adds carry designations no planetarium labels.

The ~400 stars with an IAU proper name are canonical under that name
(`"Vega"`); the HD designation resolves to the same entry. Common names
(`"Andromeda Galaxy"`, `"Tulip Nebula"`) are aliases: lookup keys, not rows.

Attribution and licence for each source are in
[`src/data/LICENSE-DATA`](../../crates/rp-catalog/src/data/LICENSE-DATA).

### What coverage bounds

Coverage bounds the quality of a *name*, never the correctness of an import.
A coordinate the catalog cannot name still becomes a target, under its
coordinates; its identity, its de-duplication and its slug are coordinate
proximity in `rp`, and none of them asks the catalog anything.

## Name resolution

`resolve(name)` returns an owned `ResolvedTarget` — canonical name, object
type, ICRS coordinate (the validated `rp_vocabulary::IcrsCoord`, so one
coordinate type runs from catalog to store to planner), magnitude, major axis
and class — or `None`.

A query is normalized before it is looked up: whitespace and dashes are
dropped, ASCII letters are lowered, and a well-known catalog prefix followed
by a digit is rewritten to its short form. `"Messier 41"`, `"m41"` and
`"M 41"` are one key, as are `"Sharpless 2-101"` and `"Sh2-101"`,
`"Barnard 33"` and `"B 33"`, `"HDE 227018"` and `"HD 227018"`.

The key table holds canonical deep-sky names, aliases and IAU star names. A
canonical name beats an alias, and of two aliases that normalize alike the
first in packing order wins (OpenNGC's common names, then the hand-kept
`aliases_curated.csv`, then SIMBAD's). Plain HD designations are not keys: an
`HD nnn` query parses the number and scans the star rows' HD column, a few
hundred thousand reads that only that query pays for.

`fuzzy_suggestions(query, limit)` returns the canonical names within a
Levenshtein distance of 3 of a query that missed, for a "did you mean" list.
It searches the key table, so it never suggests an HD number: `HD 227019` for
a typo of `HD 227018` helps nobody.

The normalization exists twice — `normalize` in `src/lib.rs` and `normalize`
in `scripts/pack_catalog.py`, which builds the key table the Rust side
searches — and the two must agree byte for byte. The
`normalize_matches_packer` test pins a shared list of inputs.

## Nearest-neighbor naming

```rust
pub struct NearestTolerances { pub dso_arcmin: f64, pub star_arcmin: f64 }

pub struct NearestMatch {
    pub target: ResolvedTarget,
    pub separation_arcmin: f64,
    pub east_offset_arcmin: f64,   // of the query from the entry: Δα·cos δ
    pub north_offset_arcmin: f64,  // Δδ
}

pub fn nearest(&self, coord: &IcrsCoord, tolerances: &NearestTolerances)
    -> Option<NearestMatch>;
```

Each class has its own acceptance radius, and **a deep-sky hit outranks any
star hit, whatever the separations**:

1. The closest deep-sky object within `dso_arcmin` wins.
2. Failing that, the closest star within `star_arcmin` wins.
3. Failing that, there is no match.

The asymmetry is what makes the star layer safe to have. The HD catalogue
averages about 0.6 stars per 10′ cone, and several times that in the galactic
plane, so a single nearest-wins search would hand the name of a nebula
framing to a field star exactly where the nebulae are. With the tiers, a star
names a target only when nothing deep-sky is in reach, and the tight star
radius fits the gesture it serves: centring on a faint star to anchor a
framing. `rp` supplies the radii from `target_store.import` (10′ and 2′ by
default).

Separation orders the hits within a class. Entries at identical coordinates —
M 42 is NGC 1976 — fall back to catalog rank and then to name, so the answer
is the same every run and the household designation wins: Messier, NGC, IC,
Sharpless, Abell, vdB, RCW, Gum, Cederblad, Barnard, LDN, LBN, Arp, Hickson,
Collinder, Melotte, Stock, Trumpler. The rank is the order of the `CATALOGS`
list in `scripts/pack_catalog.py`. Stars at identical coordinates order by HD
number.

The search itself is a binary search for the declination band in a
declination-sorted column, then an exact great-circle test (haversine, with
Δα wrapped to ±180°) on the rows inside it.

## The packed blob

All data is one little-endian file, `src/data/catalog.bin` (format
`RPCAT001`, ~5.5 MB), embedded with `include_bytes!` and read in place:
columns of fixed-width fields per class, a key table sorted by normalized
key, a table of IAU names by HD number, and a pool of length-prefixed
strings. Lookups binary-search the bytes, a star's name is formatted from its
HD number on demand, and the only allocation per call is the `ResolvedTarget`
handed back. The byte layout is documented once, in the docstring of
[`scripts/pack_catalog.py`](../../crates/rp-catalog/scripts/pack_catalog.py),
which writes it.

The shape is chosen for memory on the Raspberry Pi field rig. One struct with
owned strings per row, in a hash map, costs tens of megabytes resident at
this row count; the packed columns cost their own size, shared and read-only.
It is also why `resolve` and `nearest` return owned values: nothing can
borrow a `ResolvedTarget` from a packed row.

The readers are total. `Catalog::load_embedded` checks the magic and checks
the blob's length against the layout its header describes
(`CatalogError::BadMagic`, `Truncated`); past that, every offset computation
saturates and every read of an unaddressable position yields a miss, so a
corrupt blob degrades to "not found" and never panics or reads one section's
bytes as another's fields. `Catalog::embedded`, the process-wide instance,
logs a failed load and serves an empty catalog. The format is
[little-endian only](../workspace.md#supported-targets), like the rest of
the workspace.

## Regenerating the data

The per-catalog CSVs in `src/data/` are committed, as the reviewable input;
the star layer is fetched into an uncommitted intermediate, and only its
packed rows are committed. From the repository root:

| Script | Writes |
|--------|--------|
| `scripts/openngc_to_catalog.py` | `messier.csv`, `ngc.csv`, `ic.csv`, `aliases.csv` |
| `scripts/fetch_dso_catalogs.py` | the SIMBAD and VizieR deep-sky CSVs and `aliases_simbad.csv` |
| `scripts/fetch_iau_csn.py` | `named_stars.csv` |
| `scripts/fetch_star_layer.py` | `scripts/build/stars_iv25.csv` (uncommitted) |
| `scripts/pack_catalog.py` | `catalog.bin`, from all of the above |

The `embedded_matches_committed_csvs` test fails when the blob and the CSVs
disagree, so a CSV edited without a re-pack does not reach `main`.

## Dependencies

`rp-vocabulary` for the coordinate type, `serde` for the result types,
`thiserror` and `tracing`. Nothing depends on a network client or an async
runtime. `rp` is the only consumer of the lookups;
[`rp-targets`](rp-targets.md) stores what a lookup returned and does not
link the catalog.

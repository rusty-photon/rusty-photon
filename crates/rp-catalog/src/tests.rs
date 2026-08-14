use super::*;

fn cat() -> &'static Catalog {
    Catalog::embedded()
}

const DEFAULTS: NearestTolerances = NearestTolerances {
    dso_arcmin: 10.0,
    star_arcmin: 2.0,
};

fn coord(ra_hours: f64, dec_degrees: f64) -> IcrsCoord {
    IcrsCoord::try_new(ra_hours, dec_degrees).unwrap()
}

// --- loading & size ----------------------------------------------------

#[test]
fn loads_with_expected_size() {
    let c = cat();
    // ~18.8k DSO rows (M + NGC + IC + the #767 astrophoto catalogs)
    // plus ~353k HD stars.
    assert!(
        c.len() > 370_000,
        "catalog smaller than expected: {}",
        c.len()
    );
    assert!(!c.is_empty());
}

// `Catalog::load` reads its blob zero-copy and so takes `&'static [u8]`.
// The malformed fixtures below are therefore `static` arrays built at
// compile time: promoting a heap buffer to `'static` by leaking it is a
// genuine leak, and the nightly leak sanitizer fails the build on it.

/// All zeroes: long enough to hold a header, but the magic does not match.
static BAD_MAGIC_BLOB: [u8; 64] = [0; 64];

/// The embedded blob's real header followed by 16 bytes of body — valid
/// magic, but far fewer bytes than its row counts describe.
static TRUNCATED_BLOB: [u8; HEADER_LEN + 16] = {
    let mut blob = [0u8; HEADER_LEN + 16];
    let mut i = 0;
    while i < HEADER_LEN {
        blob[i] = CATALOG_BIN[i];
        i += 1;
    }
    blob
};

#[test]
fn load_rejects_bad_magic() {
    assert!(matches!(
        Catalog::load(&BAD_MAGIC_BLOB),
        Err(CatalogError::BadMagic)
    ));
}

#[test]
fn load_rejects_truncated_blob() {
    assert!(matches!(
        Catalog::load(&TRUNCATED_BLOB),
        Err(CatalogError::Truncated { .. })
    ));
}

/// Valid magic followed by six section counts of `u32::MAX`. The layout
/// that describes is far larger than any blob, so validation has to
/// reach its length comparison and report `Truncated` — not overflow on
/// the way there.
static ABSURD_HEADER_BLOB: [u8; HEADER_LEN] = {
    let mut blob = [0xFFu8; HEADER_LEN];
    let mut i = 0;
    while i < 8 {
        blob[i] = MAGIC[i];
        i += 1;
    }
    blob
};

#[test]
fn load_rejects_a_header_describing_more_bytes_than_can_exist() {
    assert!(matches!(
        Catalog::load(&ABSURD_HEADER_BLOB),
        Err(CatalogError::Truncated { .. })
    ));
}

/// `materialize_dso` passes `usize::MAX` for a DSO whose type index is
/// out of range, and `position` saturates to it for a field too large
/// for the target's `usize`. Both land in `pool_str`, which adds the
/// offset to the pool base — so an unchecked add panics in debug and
/// wraps onto an unrelated byte in release. It has to read as absent.
#[test]
fn pool_str_reads_an_unaddressable_offset_as_absent() {
    let c = cat();
    // The addition itself overflows.
    assert_eq!(c.pool_str(usize::MAX), "");
    // The exact boundary: `pool + off` lands one past the blob's last
    // byte, so the addition is in range and only the lookup misses.
    assert_eq!(c.pool_str(c.bytes.len() - c.pool), "");
}

/// A row reference past the end of its section must read as a miss.
/// The sections are contiguous, so an unbounded index reads the next
/// section's bytes as this section's fields rather than reading zeros,
/// and produces a fully-formed target — believable enough to act on.
#[test]
fn materialize_reads_an_out_of_range_row_reference_as_a_miss() {
    let c = cat();
    let past_dso = u32::try_from(c.dso_count).unwrap();
    let past_star = u32::try_from(c.star_count).unwrap();
    assert_eq!(c.materialize(past_dso), None);
    assert_eq!(c.materialize(STAR_BIT | past_star), None);
    // The last valid row of each section still materializes.
    assert!(c.materialize(past_dso - 1).is_some());
    assert!(c.materialize(STAR_BIT | (past_star - 1)).is_some());
}

// --- name resolution ---------------------------------------------------

#[test]
fn resolves_messier_objects_with_canonical_format() {
    let m31 = cat().resolve("M 31").expect("M 31 must resolve");
    assert_eq!(m31.name, "M 31");
    assert!(m31.object_type.starts_with('G')); // galaxy
    assert_eq!(m31.class, ObjectClass::DeepSky);
    assert!((m31.coord.ra_hours() - 0.7123).abs() < 0.001);
    assert!((m31.coord.dec_degrees() - 41.269).abs() < 0.001);
}

#[test]
fn resolves_messier_with_alternate_spellings() {
    let canon = cat().resolve("M 41").unwrap();
    for variant in ["m41", "M41", "m 41", "M  41", "Messier 41", "messier 41"] {
        let v = cat().resolve(variant).unwrap_or_else(|| {
            panic!("variant {variant:?} did not resolve");
        });
        assert_eq!(
            v, canon,
            "variant {variant:?} resolved to a different target"
        );
    }
}

#[test]
fn resolves_astrophoto_catalogs_with_alternate_spellings() {
    for (canonical, variants) in [
        (
            "Sh2-101",
            &["sh2-101", "SH2 101", "Sharpless 101", "Sharpless 2-101"][..],
        ),
        ("B 33", &["b33", "Barnard 33", "B-33"][..]),
        ("LDN 935", &["ldn935", "LDN  935"][..]),
        ("LBN 373", &["lbn 373"][..]),
        ("vdB 139", &["VDB 139", "vdb139"][..]),
        ("RCW 38", &["rcw 38"][..]),
        ("Ced 214", &["ced214"][..]),
        ("Abell 21", &["abell 21", "ABELL21"][..]),
        ("Arp 244", &["arp 244"][..]),
        ("HCG 92", &["hcg92"][..]),
        ("Cr 399", &["Collinder 399", "cr399"][..]),
        ("Mel 15", &["Melotte 15", "mel 15"][..]),
        ("Stock 2", &["stock2"][..]),
        ("Tr 37", &["Trumpler 37", "tr 37"][..]),
    ] {
        let canon = cat()
            .resolve(canonical)
            .unwrap_or_else(|| panic!("{canonical} must resolve"));
        assert_eq!(canon.name, canonical);
        assert_eq!(canon.class, ObjectClass::DeepSky);
        for v in variants {
            let got = cat()
                .resolve(v)
                .unwrap_or_else(|| panic!("variant {v:?} of {canonical} did not resolve"));
            assert_eq!(&got, &canon, "variant {v:?} resolved differently");
        }
    }
}

#[test]
fn resolves_stars_by_hd_hde_and_proper_name() {
    let vega = cat().resolve("Vega").expect("Vega must resolve");
    assert_eq!(vega.name, "Vega");
    assert_eq!(vega.class, ObjectClass::Star);
    assert_eq!(vega.object_type, "*");
    assert!((vega.coord.ra_hours() - 279.234 / 15.0).abs() < 0.001);
    assert!((vega.coord.dec_degrees() - 38.783).abs() < 0.001);
    // Tycho-2 has no VT for the brightest stars; the CSN magnitude
    // backfills named rows.
    assert!((vega.magnitude.expect("Vega magnitude") - 0.03).abs() < 0.005);
    assert_eq!(vega.size_arcmin, None);

    // The HD designation is the same entry, under the proper name.
    let by_hd = cat().resolve("HD 172167").expect("HD 172167 must resolve");
    assert_eq!(by_hd, vega);

    // Faint stars carry the designation as their name; HDE is accepted.
    let faint = cat().resolve("HD 227018").expect("HD 227018 must resolve");
    assert_eq!(faint.name, "HD 227018");
    assert_eq!(faint.class, ObjectClass::Star);
    assert_eq!(cat().resolve("HDE 227018"), Some(faint));

    assert!(cat().resolve("HD 999999").is_none());
}

#[test]
fn ngc_alias_resolves_same_object_as_messier_for_orion_nebula() {
    let m42 = cat().resolve("M 42").unwrap();
    let ngc = cat().resolve("NGC 1976").unwrap();
    assert!((m42.coord.ra_hours() - ngc.coord.ra_hours()).abs() < 1e-4);
    assert!((m42.coord.dec_degrees() - ngc.coord.dec_degrees()).abs() < 1e-4);
}

#[test]
fn ngc_resolution_is_whitespace_insensitive() {
    let by_canon = cat().resolve("NGC 224").unwrap();
    let by_squashed = cat().resolve("ngc224").unwrap();
    assert_eq!(by_canon, by_squashed);
}

#[test]
fn ic_resolves() {
    let ic1396 = cat().resolve("IC 1396").expect("IC 1396 must resolve");
    assert_eq!(ic1396.name, "IC 1396");
    // Cepheus, ~21.6h RA, ~+57.5° Dec
    assert!((ic1396.coord.ra_hours() - 21.6).abs() < 0.5);
    assert!((ic1396.coord.dec_degrees() - 57.5).abs() < 1.0);
}

#[test]
fn common_name_alias_resolves_to_canonical_ngc() {
    let andromeda = cat()
        .resolve("Andromeda Galaxy")
        .expect("alias must resolve");
    assert_eq!(andromeda.name, "NGC 224");
    let crab = cat().resolve("Crab Nebula").expect("alias must resolve");
    assert_eq!(crab.name, "NGC 1952");
    // A #767 alias pointing at a new-catalog canonical.
    let tulip = cat().resolve("Tulip Nebula").expect("alias must resolve");
    assert_eq!(tulip.name, "Sh2-101");
}

#[test]
fn missing_object_returns_none() {
    assert!(cat().resolve("M 999").is_none());
    assert!(cat().resolve("not a thing at all").is_none());
}

#[test]
fn no_panics_on_empty_or_garbage_query() {
    assert!(cat().resolve("").is_none());
    assert!(cat().resolve("   \t  ").is_none());
    assert!(cat().resolve("!!!").is_none());
}

// --- nearest() ---------------------------------------------------------

#[test]
fn nearest_breaks_exact_coordinate_ties_by_catalog_rank() {
    // M 42 and NGC 1976 are packed at identical coordinates; the
    // Messier rank must win, deterministically.
    let m42 = cat().resolve("M 42").unwrap();
    let hit = cat().nearest(&m42.coord, &DEFAULTS).expect("must hit");
    assert_eq!(hit.target.name, "M 42");
    assert!(hit.separation_arcmin < 1e-9);
    assert!(hit.east_offset_arcmin.abs() < 1e-9);
    assert!(hit.north_offset_arcmin.abs() < 1e-9);
}

#[test]
fn nearest_dso_outranks_a_dead_center_star() {
    // HD 227018 — the design doc's tap-anchor example — sits 2.3′ from
    // the Sh2-101 (Tulip Nebula) centroid. The star is dead-center in
    // its 2′ cone, but any DSO hit outranks any star hit.
    let star = cat().resolve("HD 227018").unwrap();
    let hit = cat().nearest(&star.coord, &DEFAULTS).expect("must hit");
    assert_eq!(hit.target.class, ObjectClass::DeepSky);
    assert_eq!(hit.target.name, "Sh2-101");
    assert!((hit.separation_arcmin - 2.3).abs() < 0.1);
}

#[test]
fn nearest_names_the_star_when_no_dso_is_in_its_cone() {
    let vega = cat().resolve("Vega").unwrap();
    let tol = NearestTolerances {
        dso_arcmin: 0.001,
        star_arcmin: 2.0,
    };
    let hit = cat().nearest(&vega.coord, &tol).expect("must hit Vega");
    assert_eq!(hit.target.name, "Vega");
    assert_eq!(hit.target.class, ObjectClass::Star);
    assert!(hit.separation_arcmin < 1e-9);
}

#[test]
fn nearest_reports_query_offsets_from_the_centroid() {
    let m31 = cat().resolve("M 31").unwrap();
    let dec = m31.coord.dec_degrees() + 3.0 / 60.0;
    let ra_hours = m31.coord.ra_hours() + (6.0 / 60.0) / dec.to_radians().cos() / 15.0;
    let hit = cat()
        .nearest(&coord(ra_hours, dec), &DEFAULTS)
        .expect("must hit");
    assert_eq!(hit.target.name, "M 31");
    // East = Δα·cos δ of the query FROM the centroid (the cos is taken
    // at the centroid's declination, hence the loose-ish tolerance).
    assert!((hit.east_offset_arcmin - 6.0).abs() < 0.05);
    assert!((hit.north_offset_arcmin - 3.0).abs() < 0.01);
    assert!((hit.separation_arcmin - (36.0f64 + 9.0).sqrt()).abs() < 0.05);
}

#[test]
fn nearest_misses_when_nothing_is_in_either_cone() {
    let tol = NearestTolerances {
        dso_arcmin: 0.001,
        star_arcmin: 0.001,
    };
    assert!(cat().nearest(&coord(12.0, 45.0), &tol).is_none());
}

#[test]
fn nearest_handles_zero_and_non_finite_radii() {
    let m42 = cat().resolve("M 42").unwrap();
    let zero = NearestTolerances {
        dso_arcmin: 0.0,
        star_arcmin: 0.0,
    };
    assert!(cat().nearest(&m42.coord, &zero).is_none());
    let nan = NearestTolerances {
        dso_arcmin: f64::NAN,
        star_arcmin: -1.0,
    };
    assert!(cat().nearest(&m42.coord, &nan).is_none());
}

#[test]
fn nearest_is_deterministic() {
    let m42 = cat().resolve("M 42").unwrap();
    let a = cat().nearest(&m42.coord, &DEFAULTS).unwrap();
    let b = cat().nearest(&m42.coord, &DEFAULTS).unwrap();
    assert_eq!(a, b);
}

#[test]
fn nearest_works_at_the_celestial_pole() {
    // NGC 3172 ("Polarissima Borealis") is the closest NGC to the pole;
    // the dec-band scan must not fall over at the clamp.
    let hit = cat()
        .nearest(
            &coord(0.0, 90.0),
            &NearestTolerances {
                dso_arcmin: 120.0,
                star_arcmin: 2.0,
            },
        )
        .expect("something within 2° of the pole");
    assert_eq!(hit.target.class, ObjectClass::DeepSky);
    assert!(hit.separation_arcmin <= 120.0);
}

#[test]
fn separation_wraps_across_ra_zero() {
    // 0.1° on either side of RA 0 at the equator = 12′ apart.
    let sep = separation_arcmin(359.9, 0.0, 0.1, 0.0);
    assert!((sep - 12.0).abs() < 1e-9, "sep {sep}");
    assert!((ra_delta_degrees(359.9, 0.1) - 0.2).abs() < 1e-9);
    assert!((ra_delta_degrees(0.1, 359.9) + 0.2).abs() < 1e-9);
}

// --- fuzzy suggestions ---------------------------------------------------

#[test]
fn fuzzy_suggestions_finds_close_neighbours() {
    let suggestions = cat().fuzzy_suggestions("M 41", 5);
    assert!(
        suggestions.iter().any(|s| s == "M 41"),
        "exact match should appear in fuzzy list as canonical name: {suggestions:?}"
    );
    let typo = cat().fuzzy_suggestions("M 411", 5);
    assert!(
        typo.iter().any(|s| s == "M 41"),
        "typo M 411 should suggest M 41 by canonical name: {typo:?}"
    );
}

#[test]
fn fuzzy_suggestions_dedup_canonical_names() {
    // "Andromeda Galaxy" is an alias of NGC 224; a query close to
    // it should yield the canonical "NGC 224" once, not twice.
    let suggestions = cat().fuzzy_suggestions("NGC 224", 10);
    let copies = suggestions.iter().filter(|s| *s == "NGC 224").count();
    assert_eq!(
        copies, 1,
        "expected canonical name once, got {copies}: {suggestions:?}"
    );
}

#[test]
fn fuzzy_suggestions_cover_proper_star_names_but_not_hd_designations() {
    let suggestions = cat().fuzzy_suggestions("Vga", 5);
    assert!(
        suggestions.iter().any(|s| s == "Vega"),
        "typo Vga should suggest Vega: {suggestions:?}"
    );
    // Plain HD designations are resolved numerically, never suggested.
    let hd = cat().fuzzy_suggestions("HD 227018", 5);
    assert!(
        hd.iter().all(|s| !s.starts_with("HD ")),
        "HD designations must not be suggested: {hd:?}"
    );
}

// --- embedded data stays in sync with its committed sources -------------

#[test]
fn embedded_matches_committed_csvs() {
    #[derive(Debug, serde::Deserialize)]
    struct CsvRow {
        name: String,
        #[serde(rename = "type")]
        object_type: String,
        ra_hours: f64,
        dec_degrees: f64,
        magnitude: String,
        size_arcmin: String,
    }

    let files: [(&str, &str); 18] = [
        ("messier.csv", include_str!("data/messier.csv")),
        ("ngc.csv", include_str!("data/ngc.csv")),
        ("ic.csv", include_str!("data/ic.csv")),
        ("sh2.csv", include_str!("data/sh2.csv")),
        ("abell.csv", include_str!("data/abell.csv")),
        ("vdb.csv", include_str!("data/vdb.csv")),
        ("rcw.csv", include_str!("data/rcw.csv")),
        ("gum.csv", include_str!("data/gum.csv")),
        ("ced.csv", include_str!("data/ced.csv")),
        ("barnard.csv", include_str!("data/barnard.csv")),
        ("ldn.csv", include_str!("data/ldn.csv")),
        ("lbn.csv", include_str!("data/lbn.csv")),
        ("arp.csv", include_str!("data/arp.csv")),
        ("hcg.csv", include_str!("data/hcg.csv")),
        ("collinder.csv", include_str!("data/collinder.csv")),
        ("melotte.csv", include_str!("data/melotte.csv")),
        ("stock.csv", include_str!("data/stock.csv")),
        ("trumpler.csv", include_str!("data/trumpler.csv")),
    ];

    let mut rows = 0usize;
    for (label, body) in files {
        for record in csv::Reader::from_reader(body.as_bytes()).deserialize::<CsvRow>() {
            let r: CsvRow = record.unwrap_or_else(|e| panic!("{label}: {e}"));
            let t = cat()
                .resolve(&r.name)
                .unwrap_or_else(|| panic!("{label}: {} not in blob", r.name));
            assert_eq!(t.name, r.name, "{label}: canonical name drifted");
            assert_eq!(t.class, ObjectClass::DeepSky);
            assert_eq!(t.object_type, r.object_type, "{label}: {} type", r.name);
            assert!(
                (t.coord.ra_hours() - r.ra_hours).abs() < 1e-5,
                "{label}: {} RA drifted: {} vs {}",
                r.name,
                t.coord.ra_hours(),
                r.ra_hours
            );
            assert!(
                (t.coord.dec_degrees() - r.dec_degrees).abs() < 1e-5,
                "{label}: {} Dec drifted",
                r.name
            );
            match r.magnitude.trim().parse::<f64>() {
                Ok(m) => {
                    let got = t
                        .magnitude
                        .unwrap_or_else(|| panic!("{label}: {} lost its magnitude", r.name));
                    assert!(
                        (got - m).abs() < 0.005 + 1e-9,
                        "{label}: {} magnitude",
                        r.name
                    );
                }
                Err(_) => assert_eq!(t.magnitude, None, "{label}: {} magnitude", r.name),
            }
            match r.size_arcmin.trim().parse::<f64>() {
                // 0.0 sizes in the source collapse onto the "none"
                // sentinel; anything real survives to 0.05'.
                Ok(s) if s > 0.05 => {
                    let got = t
                        .size_arcmin
                        .unwrap_or_else(|| panic!("{label}: {} lost its size", r.name));
                    assert!((got - s).abs() < 0.05 + 1e-9, "{label}: {} size", r.name);
                }
                _ => {}
            }
            rows += 1;
        }
    }
    assert!(
        rows > 18_000,
        "expected the full DSO complement, got {rows}"
    );
}

#[test]
fn embedded_matches_committed_aliases() {
    #[derive(Debug, serde::Deserialize)]
    struct AliasRow {
        alias: String,
        canonical_name: String,
    }
    for (label, body) in [
        ("aliases.csv", include_str!("data/aliases.csv")),
        (
            "aliases_curated.csv",
            include_str!("data/aliases_curated.csv"),
        ),
        (
            "aliases_simbad.csv",
            include_str!("data/aliases_simbad.csv"),
        ),
    ] {
        for record in csv::Reader::from_reader(body.as_bytes()).deserialize::<AliasRow>() {
            let r: AliasRow = record.unwrap_or_else(|e| panic!("{label}: {e}"));
            // Ambiguous aliases keep their first mapping, so the target
            // may differ from this row — but every alias must resolve.
            assert!(
                cat().resolve(&r.alias).is_some(),
                "{label}: alias {:?} does not resolve",
                r.alias
            );
            assert!(
                cat().resolve(&r.canonical_name).is_some(),
                "{label}: canonical {:?} does not resolve",
                r.canonical_name
            );
        }
    }
}

#[test]
fn embedded_matches_committed_named_stars() {
    #[derive(Debug, serde::Deserialize)]
    struct NamedRow {
        name: String,
        hd: u32,
    }
    let mut rows = 0usize;
    for record in csv::Reader::from_reader(include_bytes!("data/named_stars.csv").as_slice())
        .deserialize::<NamedRow>()
    {
        let r: NamedRow = record.unwrap();
        let by_name = cat()
            .resolve(&r.name)
            .unwrap_or_else(|| panic!("named star {} not in blob", r.name));
        assert_eq!(by_name.class, ObjectClass::Star, "{}", r.name);
        assert_eq!(by_name.name, r.name);
        let by_hd = cat()
            .resolve(&format!("HD {}", r.hd))
            .unwrap_or_else(|| panic!("HD {} not in blob", r.hd));
        assert_eq!(by_hd, by_name, "HD {} != {}", r.hd, r.name);
        rows += 1;
    }
    assert!(rows > 400, "expected the IAU CSN complement, got {rows}");
}

// --- normalization -------------------------------------------------------

#[test]
fn normalize_handles_prefix_variants() {
    assert_eq!(normalize("Messier 41"), "m41");
    assert_eq!(normalize("MESSIER41"), "m41");
    assert_eq!(normalize("M 41"), "m41");
    assert_eq!(normalize("Sharpless 101"), "sh2101");
    assert_eq!(normalize("Sharpless 2-101"), "sh2101");
    assert_eq!(normalize("Sh2-101"), "sh2101");
    assert_eq!(normalize("Barnard 33"), "b33");
    assert_eq!(normalize("B-33"), "b33");
    assert_eq!(normalize("Collinder 399"), "cr399");
    assert_eq!(normalize("Melotte 15"), "mel15");
    assert_eq!(normalize("Trumpler 37"), "tr37");
    assert_eq!(normalize("HDE 227018"), "hd227018");
    // Prefixes not followed by a digit are left alone.
    assert_eq!(normalize("messieRR"), "messierr");
    assert_eq!(normalize("Crab Nebula"), "crabnebula");
    assert_eq!(normalize("Barnard's Star"), "barnard'sstar");
}

#[test]
fn levenshtein_capped() {
    assert_eq!(levenshtein("kitten", "sitting", 100), 3);
    assert_eq!(levenshtein("kitten", "sitting", 2), 2);
    assert_eq!(levenshtein("", "abc", 4), 3);
    assert_eq!(levenshtein("abc", "", 4), 3);
    assert_eq!(levenshtein("abc", "abc", 4), 0);
}

// --- serde shape ---------------------------------------------------------

#[test]
fn object_class_serializes_snake_case() {
    assert_eq!(
        serde_json::to_string(&ObjectClass::DeepSky).unwrap(),
        "\"deep_sky\""
    );
    assert_eq!(
        serde_json::to_string(&ObjectClass::Star).unwrap(),
        "\"star\""
    );
}

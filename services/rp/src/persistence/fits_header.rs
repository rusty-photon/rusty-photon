//! The standard FITS keywords `capture` writes into each frame's primary
//! header — `docs/services/rp.md` §"FITS header".
//!
//! The header is an export of the exposure document for other
//! programs: every card below but `ROWORDER` is projected from an
//! [`ExposureDocument`] field and from nothing else, so the header
//! carries no fact the sidecar lacks. `ROWORDER` describes the file's
//! pixel array, not the exposure. The sidecar is the record, and `rp`
//! never reads a fact back from the header (rp.md §"Ground rules").
//! A card whose source is absent is left out, never written as a
//! placeholder. Free text has its typographic punctuation folded to
//! ASCII; a value `rp-fits` still refuses (`Hα`, one longer than a
//! card) drops only that card — no header problem fails a capture. `DOC_ID` is not built here: [`super::fits`] stamps
//! it on every write.

use chrono::{DateTime, SecondsFormat, Utc};
use rp_fits::writer::{fold_to_ascii, real_as_written, Keyword, KeywordValue};
use rp_vocabulary::FrameType;
use tracing::debug;

use super::document::{ExposureDocument, SensorType};

/// The FITS form of `DATE-OBS` (UTC, millisecond precision, no zone).
const FITS_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f";

/// Format an exposure-start instant for the document's
/// `exposure_started_at`: RFC 3339 at millisecond precision, the same
/// precision `DATE-OBS` carries.
#[must_use]
pub fn document_timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, false)
}

/// The standard keyword cards for one frame, in header order.
#[must_use]
pub fn header_keywords(doc: &ExposureDocument) -> Vec<Keyword> {
    let mut cards = Cards::default();

    // `capture` writes the camera's row 0 first, and ASCOM has `Y` grow
    // from top to bottom.
    cards.string(
        "ROWORDER",
        Some("TOP-DOWN"),
        "first row is the top of the image",
    );
    if let Some(date_obs) = doc.exposure_started_at.as_deref().and_then(fits_timestamp) {
        cards.push(
            "DATE-OBS",
            KeywordValue::Str(date_obs),
            "[UTC] exposure start",
        );
    }
    if let Some(duration) = doc.duration {
        cards.real("EXPTIME", Some(duration.as_secs_f64()), "[s] exposure time");
    }
    if let Some(frame_type) = doc.frame_type {
        cards.string("IMAGETYP", Some(image_type(frame_type)), "frame type");
    }
    if let Some(target) = &doc.target {
        cards.string("OBJECT", target.display_name.as_deref(), "target name");
        cards.string(
            "OBJCTRA",
            target.ra_hours.and_then(sexagesimal_ra).as_deref(),
            "[hms J2000] target right ascension",
        );
        cards.string(
            "OBJCTDEC",
            target.dec_degrees.and_then(sexagesimal_dec).as_deref(),
            "[dms J2000] target declination",
        );
    }
    if let Some(pointing) = doc.pointing {
        // Into [0, 360): a mount on the 0h seam can report 24h or a hair
        // below zero, and readers expect the canonical range.
        cards.real(
            "RA",
            Some(canonical_ra_degrees(pointing.ra_hours)),
            "[deg] mount right ascension",
        );
        cards.real(
            "DEC",
            Some(pointing.dec_degrees).filter(|dec| dec.abs() <= 90.0),
            "[deg] mount declination",
        );
    }
    cards.string("INSTRUME", doc.camera_name.as_deref(), "camera");
    cards.string("TELESCOP", doc.telescope.as_deref(), "telescope");
    cards.string("FILTER", doc.filter.as_deref(), "filter");
    cards.integer("GAIN", doc.gain, "camera gain");
    cards.integer("OFFSET", doc.offset, "camera offset");
    cards.string(
        "READOUTM",
        doc.readout_mode_name.as_deref(),
        "camera readout mode",
    );
    // No `EGAIN`: `electrons_per_adu` counts the driver's ADU, which need
    // not be the file's (rp.md §"FITS header").
    cards.real(
        "CCD-TEMP",
        doc.sensor_temperature_c,
        "[degC] sensor temperature",
    );
    cards.real(
        "SET-TEMP",
        doc.cooler_setpoint_c.map(f64::from),
        "[degC] cooler setpoint",
    );
    cards.geometry(doc);
    cards.mosaic(doc);
    if let Some(site) = doc.site {
        cards.real(
            "SITELAT",
            Some(site.latitude_degrees),
            "[deg] site latitude, north positive",
        );
        cards.real(
            "SITELONG",
            Some(site.longitude_degrees),
            "[deg] site longitude, east positive",
        );
    }
    let software = doc
        .rp_version
        .as_deref()
        .map(|version| format!("rusty-photon rp {version}"));
    cards.string("SWCREATE", software.as_deref(), "creating software");

    cards.0
}

/// The accumulating card list. Each `push` validates through
/// [`Keyword::new`] and drops — with a `debug!` — a card `rp-fits`
/// refuses.
#[derive(Default)]
struct Cards(Vec<Keyword>);

impl Cards {
    /// The binning and the `optics` block's cards: `XBINNING`/`YBINNING`,
    /// the binned pixel, and the light path's focal length, aperture
    /// and focal ratio.
    fn geometry(&mut self, doc: &ExposureDocument) {
        let optics = doc.optics.as_ref();
        if let Some(binning) = doc.binning {
            self.integer("XBINNING", Some(binning.x), "binning factor");
            self.integer("YBINNING", Some(binning.y), "binning factor");
            // The binned pixel — the MaxIm convention plate solvers read
            // `XPIXSZ` by (rp.md §"FITS header").
            self.real(
                "XPIXSZ",
                optics
                    .and_then(|o| o.pixel_size_x_um)
                    .map(|um| um * f64::from(binning.x)),
                "[um] binned pixel width",
            );
            self.real(
                "YPIXSZ",
                optics
                    .and_then(|o| o.pixel_size_y_um)
                    .map(|um| um * f64::from(binning.y)),
                "[um] binned pixel height",
            );
        }
        self.real(
            "FOCALLEN",
            optics.and_then(|o| o.focal_length_mm),
            "[mm] focal length",
        );
        self.real(
            "APTDIA",
            optics.and_then(|o| o.aperture_mm),
            "[mm] aperture diameter",
        );
        self.real(
            "FOCRATIO",
            optics.and_then(super::document::Optics::focal_ratio),
            "focal ratio",
        );
    }

    /// `BAYERPAT`, with `XBAYROFF`/`YBAYROFF` written as 0 beside it so a
    /// reader that applies them and one that ignores them agree.
    fn mosaic(&mut self, doc: &ExposureDocument) {
        if let Some(pattern) = bayer_pattern(doc) {
            self.string(
                "BAYERPAT",
                Some(pattern),
                "Bayer pattern at the first pixel, top-down",
            );
            self.integer("XBAYROFF", Some(0), "[px] already applied to BAYERPAT");
            self.integer("YBAYROFF", Some(0), "[px] already applied to BAYERPAT");
        }
    }

    fn push(&mut self, key: &'static str, value: KeywordValue, comment: &'static str) {
        match Keyword::new(key, value) {
            Ok(card) => self.0.push(card.with_comment(comment)),
            Err(e) => debug!(keyword = key, error = %e, "dropping FITS header card"),
        }
    }

    fn string(&mut self, key: &'static str, value: Option<&str>, comment: &'static str) {
        if let Some(value) = value {
            self.push(key, KeywordValue::Str(fold_to_ascii(value)), comment);
        }
    }

    fn real(&mut self, key: &'static str, value: Option<f64>, comment: &'static str) {
        if let Some(value) = value {
            self.push(key, KeywordValue::Float(value), comment);
        }
    }

    fn integer<T: Into<i64>>(
        &mut self,
        key: &'static str,
        value: Option<T>,
        comment: &'static str,
    ) {
        if let Some(value) = value {
            self.push(key, KeywordValue::Int(value.into()), comment);
        }
    }
}

/// The 2×2 pattern at the file's first pixel, read top-down. ASCOM names
/// the `RGGB` matrix and puts its upper-left corner, the `R`, at the
/// Bayer offsets from the sensor's pixel (0, 0); `capture` starts every
/// frame at that pixel, so the offsets apply to the file directly.
///
/// `None` for any other sensor type, for offsets that are unknown or
/// outside the 2×2 matrix, and for a binned frame: whether a binned frame
/// keeps the mosaic depends on how the camera bins, which ASCOM does not
/// report (rp.md §"FITS header").
fn bayer_pattern(doc: &ExposureDocument) -> Option<&'static str> {
    if doc.sensor_type != Some(SensorType::Rggb)
        || doc.binning != Some(rp_vocabulary::Binning { x: 1, y: 1 })
    {
        return None;
    }
    match (doc.bayer_offset_x?, doc.bayer_offset_y?) {
        (0, 0) => Some("RGGB"),
        (1, 0) => Some("GRBG"),
        (0, 1) => Some("GBRG"),
        (1, 1) => Some("BGGR"),
        (x, y) => {
            debug!(
                bayer_offset_x = x,
                bayer_offset_y = y,
                "Bayer offsets outside a 2x2 matrix; omitting BAYERPAT"
            );
            None
        }
    }
}

/// `exposure_started_at` (RFC 3339) in `DATE-OBS`'s FITS form. `None`
/// for a value that does not parse — the card is then left out.
fn fits_timestamp(rfc3339: &str) -> Option<String> {
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(at) => Some(
            at.with_timezone(&Utc)
                .format(FITS_TIMESTAMP_FORMAT)
                .to_string(),
        ),
        Err(e) => {
            debug!(value = rfc3339, error = %e, "exposure_started_at is not RFC 3339; omitting DATE-OBS");
            None
        }
    }
}

/// Mount RA in degrees, wrapped into [0, 360) as the card will print it:
/// a mount on the 0h seam can report 24h or a hair below zero, and a
/// value just under 360 — `rem_euclid` of a tiny negative among them —
/// prints as `3.6000000000E+02` at the card's 11 significant digits. (A
/// `-0.0` the writer prints as zero, as it does for every real card.)
fn canonical_ra_degrees(ra_hours: f64) -> f64 {
    let degrees = (ra_hours * 15.0).rem_euclid(360.0);
    if real_as_written(degrees) >= 360.0 {
        0.0
    } else {
        degrees
    }
}

/// `IMAGETYP` in the `MaxIm DL` spelling every stacker recognizes.
const fn image_type(frame_type: FrameType) -> &'static str {
    match frame_type {
        FrameType::Light => "Light Frame",
        FrameType::Dark => "Dark Frame",
        FrameType::Flat => "Flat Field",
        FrameType::Bias => "Bias Frame",
    }
}

/// Right ascension as `HH MM SS.ss`. Rounded to the centisecond of time
/// first, so a value a hair under the next minute carries into it rather
/// than printing `60.00`; 24h wraps to 0h. `None` for a non-finite
/// input.
fn sexagesimal_ra(ra_hours: f64) -> Option<String> {
    const PER_HOUR: f64 = 360_000.0;
    const PER_MINUTE: f64 = 6_000.0;
    const PER_DAY: f64 = 24.0 * PER_HOUR;
    if !ra_hours.is_finite() {
        return None;
    }
    let mut total = (ra_hours.rem_euclid(24.0) * PER_HOUR).round();
    if total >= PER_DAY {
        total = 0.0;
    }
    // Whole numbers below 2^53 throughout, so every step is exact.
    let hours = (total / PER_HOUR).floor();
    let rest = hours.mul_add(-PER_HOUR, total);
    let minutes = (rest / PER_MINUTE).floor();
    let seconds = minutes.mul_add(-PER_MINUTE, rest) / 100.0;
    Some(format!("{hours:02.0} {minutes:02.0} {seconds:05.2}"))
}

/// Declination as `+DD MM SS.s`, rounded to the tenth of an arcsecond
/// first for the same carry reason as [`sexagesimal_ra`]. `None` for a
/// non-finite value or one outside ±90°.
fn sexagesimal_dec(dec_degrees: f64) -> Option<String> {
    const PER_DEGREE: f64 = 36_000.0;
    const PER_MINUTE: f64 = 600.0;
    if !dec_degrees.is_finite() || dec_degrees.abs() > 90.0 {
        return None;
    }
    let total = (dec_degrees.abs() * PER_DEGREE).round();
    // A value that rounds to zero prints as `+00 00 00.0`, never `-`.
    let sign = if dec_degrees < 0.0 && total > 0.0 {
        '-'
    } else {
        '+'
    };
    let degrees = (total / PER_DEGREE).floor();
    let rest = degrees.mul_add(-PER_DEGREE, total);
    let minutes = (rest / PER_MINUTE).floor();
    let seconds = minutes.mul_add(-PER_MINUTE, rest) / 10.0;
    Some(format!(
        "{sign}{degrees:02.0} {minutes:02.0} {seconds:04.1}"
    ))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::persistence::document::{
        ExposureSite, ExposureTarget, MountPointing, Optics, RP_VERSION,
    };

    /// The card list as `(key, value)` pairs, keys trimmed.
    fn cards(doc: &ExposureDocument) -> Vec<(String, KeywordValue)> {
        let mut buf = Vec::new();
        rp_fits::writer::write_u8_image(&mut buf, &[0], 1, 1, &header_keywords(doc)).unwrap();
        let mut out = Vec::new();
        for card in buf.chunks(80) {
            let key = std::str::from_utf8(&card[..8])
                .unwrap()
                .trim_end()
                .to_string();
            if key == "END" {
                break;
            }
            if ["SIMPLE", "BITPIX", "NAXIS", "NAXIS1", "NAXIS2"].contains(&key.as_str()) {
                continue;
            }
            let value = rp_fits::reader::read_primary_keyword(std::io::Cursor::new(&buf), &key)
                .unwrap()
                .unwrap();
            out.push((key, value));
        }
        out
    }

    fn value_of(cards: &[(String, KeywordValue)], key: &str) -> Option<KeywordValue> {
        cards.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn keys(cards: &[(String, KeywordValue)]) -> Vec<&str> {
        cards.iter().map(|(k, _)| k.as_str()).collect()
    }

    fn full_document() -> ExposureDocument {
        ExposureDocument {
            id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            captured_at: "2026-03-02T01:20:02+00:00".to_string(),
            exposure_started_at: Some("2026-03-02T01:15:00.412+00:00".to_string()),
            file_path: "/data/550e8400.fits".to_string(),
            width: 2,
            height: 2,
            camera_id: Some("main-cam".to_string()),
            camera_name: Some("QHY600M".to_string()),
            train_id: Some("main".to_string()),
            telescope: Some("Takahashi FSQ-106EDX4".to_string()),
            duration: Some(Duration::from_secs(300)),
            binning: Some(rp_vocabulary::Binning { x: 2, y: 2 }),
            filter: Some("Luminance".to_string()),
            gain: Some(26),
            offset: Some(30),
            readout_mode: Some(1),
            readout_mode_name: Some("High Gain Mode 16BIT".to_string()),
            electrons_per_adu: Some(0.8),
            max_adu: Some(65535),
            cooler_setpoint_c: Some(-10),
            sensor_temperature_c: Some(-9.8),
            pointing: Some(MountPointing {
                ra_hours: 0.7121,
                dec_degrees: 41.2702,
            }),
            site: Some(ExposureSite {
                latitude_degrees: 47.6062,
                longitude_degrees: -122.3321,
            }),
            rp_version: Some(RP_VERSION.to_string()),
            optics: Some(Optics {
                focal_length_mm: Some(530.0),
                aperture_mm: Some(106.0),
                pixel_size_x_um: Some(3.76),
                pixel_size_y_um: Some(3.76),
                ..Optics::default()
            }),
            target: Some(ExposureTarget {
                slug: "m31".to_string(),
                display_name: Some("M31".to_string()),
                ra_hours: Some(0.7123),
                dec_degrees: Some(41.2689),
            }),
            frame_type: Some(FrameType::Light),
            ..ExposureDocument::default()
        }
    }

    /// The full document with its optics block replaced.
    fn with_optics(optics: Optics) -> ExposureDocument {
        ExposureDocument {
            optics: Some(optics),
            ..full_document()
        }
    }

    #[test]
    fn a_full_document_yields_every_keyword_in_header_order() {
        let cards = cards(&full_document());
        assert_eq!(
            keys(&cards),
            [
                "ROWORDER", "DATE-OBS", "EXPTIME", "IMAGETYP", "OBJECT", "OBJCTRA", "OBJCTDEC",
                "RA", "DEC", "INSTRUME", "TELESCOP", "FILTER", "GAIN", "OFFSET", "READOUTM",
                "CCD-TEMP", "SET-TEMP", "XBINNING", "YBINNING", "XPIXSZ", "YPIXSZ", "FOCALLEN",
                "APTDIA", "FOCRATIO", "SITELAT", "SITELONG", "SWCREATE",
            ]
        );
    }

    #[test]
    fn values_mirror_the_document() {
        let cards = cards(&full_document());
        let expect = [
            ("ROWORDER", KeywordValue::Str("TOP-DOWN".into())),
            (
                "DATE-OBS",
                KeywordValue::Str("2026-03-02T01:15:00.412".into()),
            ),
            ("EXPTIME", KeywordValue::Float(300.0)),
            ("IMAGETYP", KeywordValue::Str("Light Frame".into())),
            ("OBJECT", KeywordValue::Str("M31".into())),
            ("OBJCTRA", KeywordValue::Str("00 42 44.28".into())),
            ("OBJCTDEC", KeywordValue::Str("+41 16 08.0".into())),
            ("INSTRUME", KeywordValue::Str("QHY600M".into())),
            (
                "TELESCOP",
                KeywordValue::Str("Takahashi FSQ-106EDX4".into()),
            ),
            ("FILTER", KeywordValue::Str("Luminance".into())),
            ("GAIN", KeywordValue::Int(26)),
            ("OFFSET", KeywordValue::Int(30)),
            ("READOUTM", KeywordValue::Str("High Gain Mode 16BIT".into())),
            ("CCD-TEMP", KeywordValue::Float(-9.8)),
            ("SET-TEMP", KeywordValue::Float(-10.0)),
            ("XBINNING", KeywordValue::Int(2)),
            ("YBINNING", KeywordValue::Int(2)),
            ("FOCALLEN", KeywordValue::Float(530.0)),
            ("APTDIA", KeywordValue::Float(106.0)),
            ("XPIXSZ", KeywordValue::Float(7.52)),
            ("YPIXSZ", KeywordValue::Float(7.52)),
            ("FOCRATIO", KeywordValue::Float(5.0)),
            ("SITELAT", KeywordValue::Float(47.6062)),
            ("SITELONG", KeywordValue::Float(-122.3321)),
        ];
        for (key, want) in expect {
            assert_eq!(value_of(&cards, key), Some(want), "{key}");
        }
        assert_eq!(
            value_of(&cards, "SWCREATE"),
            Some(KeywordValue::Str(format!(
                "rusty-photon rp {}",
                env!("CARGO_PKG_VERSION")
            )))
        );
    }

    #[test]
    fn ra_dec_are_the_pointing_in_degrees() {
        let cards = cards(&full_document());
        let ra = value_of(&cards, "RA").and_then(|v| v.as_real()).unwrap();
        let dec = value_of(&cards, "DEC").and_then(|v| v.as_real()).unwrap();
        assert!((ra - 0.7121 * 15.0).abs() < 1e-9, "RA {ra}");
        assert!((dec - 41.2702).abs() < 1e-9, "DEC {dec}");
    }

    #[test]
    fn a_train_without_a_telescope_or_aperture_carries_neither_card() {
        let mut doc = with_optics(Optics {
            focal_length_mm: Some(530.0),
            ..Optics::default()
        });
        doc.telescope = None;
        let cards = cards(&doc);
        for key in ["TELESCOP", "APTDIA", "FOCRATIO"] {
            assert_eq!(value_of(&cards, key), None, "{key}");
        }
        assert_eq!(
            value_of(&cards, "FOCALLEN"),
            Some(KeywordValue::Float(530.0))
        );
    }

    #[test]
    fn ra_wraps_into_the_canonical_range() {
        let mut doc = full_document();
        for (ra_hours, want) in [
            (24.0, 0.0),
            (-0.0001, 359.9985),
            (-1e-15, 0.0),
            // Below 360 as an f64, but 360 at the card's 11 digits.
            (-1e-12, 0.0),
        ] {
            doc.pointing = Some(MountPointing {
                ra_hours,
                dec_degrees: 0.0,
            });
            let cards = cards(&doc);
            let ra = value_of(&cards, "RA").and_then(|v| v.as_real()).unwrap();
            assert!((ra - want).abs() < 1e-9, "RA {ra} for {ra_hours}h");
        }
    }

    #[test]
    fn a_declination_beyond_the_pole_drops_only_dec() {
        let mut doc = full_document();
        doc.pointing = Some(MountPointing {
            ra_hours: 1.0,
            dec_degrees: 90.5,
        });
        let cards = cards(&doc);
        assert_eq!(value_of(&cards, "DEC"), None);
        assert!(value_of(&cards, "RA").is_some());
    }

    #[test]
    fn pixel_size_is_scaled_by_the_binning() {
        let cards = cards(&with_optics(Optics {
            pixel_size_x_um: Some(3.76),
            pixel_size_y_um: Some(2.0),
            ..Optics::default()
        }));
        let x = value_of(&cards, "XPIXSZ")
            .and_then(|v| v.as_real())
            .unwrap();
        let y = value_of(&cards, "YPIXSZ")
            .and_then(|v| v.as_real())
            .unwrap();
        assert!((x - 7.52).abs() < 1e-9, "XPIXSZ {x}");
        assert!((y - 4.0).abs() < 1e-9, "YPIXSZ {y}");
    }

    #[test]
    fn a_bare_document_yields_only_the_row_order() {
        let cards = cards(&ExposureDocument::default());
        assert_eq!(keys(&cards), ["ROWORDER"]);
    }

    /// An unbinned frame from an `RGGB`-type sensor with these offsets.
    fn colour_document(bayer_offset_x: u8, bayer_offset_y: u8) -> ExposureDocument {
        ExposureDocument {
            sensor_type: Some(SensorType::Rggb),
            bayer_offset_x: Some(bayer_offset_x),
            bayer_offset_y: Some(bayer_offset_y),
            binning: Some(rp_vocabulary::Binning { x: 1, y: 1 }),
            ..full_document()
        }
    }

    const BAYER_KEYS: [&str; 3] = ["BAYERPAT", "XBAYROFF", "YBAYROFF"];

    #[test]
    fn an_unbinned_mosaic_carries_the_pattern_at_its_first_pixel() {
        // ASCOM's matrix with its `R` at the offsets; the ASI120MC-S
        // reports (1, 0), which ZWO's SDK names GRBG.
        for (x, y, want) in [
            (0, 0, "RGGB"),
            (1, 0, "GRBG"),
            (0, 1, "GBRG"),
            (1, 1, "BGGR"),
        ] {
            let cards = cards(&colour_document(x, y));
            assert_eq!(
                value_of(&cards, "BAYERPAT"),
                Some(KeywordValue::Str(want.into())),
                "offsets ({x}, {y})"
            );
            assert_eq!(value_of(&cards, "XBAYROFF"), Some(KeywordValue::Int(0)));
            assert_eq!(value_of(&cards, "YBAYROFF"), Some(KeywordValue::Int(0)));
        }
    }

    #[test]
    fn the_bayer_cards_follow_the_pixel_geometry() {
        let cards = cards(&colour_document(1, 0));
        let keys = keys(&cards);
        let at = keys.iter().position(|k| *k == "FOCRATIO").unwrap();
        assert_eq!(keys[at + 1..at + 4], BAYER_KEYS);
    }

    #[test]
    fn a_binned_mosaic_carries_no_bayer_cards() {
        for binning in [
            Some(rp_vocabulary::Binning { x: 2, y: 2 }),
            Some(rp_vocabulary::Binning { x: 1, y: 2 }),
            None,
        ] {
            let doc = ExposureDocument {
                binning,
                ..colour_document(1, 0)
            };
            let cards = cards(&doc);
            for key in BAYER_KEYS {
                assert_eq!(value_of(&cards, key), None, "{key} at {binning:?}");
            }
        }
    }

    #[test]
    fn only_an_rggb_type_sensor_carries_bayer_cards() {
        for sensor_type in [
            None,
            Some(SensorType::Monochrome),
            Some(SensorType::Color),
            Some(SensorType::Cmyg),
            Some(SensorType::Cmyg2),
            Some(SensorType::Lrgb),
        ] {
            let doc = ExposureDocument {
                sensor_type,
                ..colour_document(0, 0)
            };
            let cards = cards(&doc);
            for key in BAYER_KEYS {
                assert_eq!(value_of(&cards, key), None, "{key} for {sensor_type:?}");
            }
        }
    }

    #[test]
    fn unknown_or_out_of_matrix_offsets_carry_no_bayer_cards() {
        let mut missing_x = colour_document(0, 0);
        missing_x.bayer_offset_x = None;
        let mut missing_y = colour_document(0, 0);
        missing_y.bayer_offset_y = None;
        for doc in [
            missing_x,
            missing_y,
            colour_document(2, 0),
            colour_document(0, 3),
        ] {
            let cards = cards(&doc);
            for key in BAYER_KEYS {
                assert_eq!(
                    value_of(&cards, key),
                    None,
                    "{key} for ({:?}, {:?})",
                    doc.bayer_offset_x,
                    doc.bayer_offset_y
                );
            }
        }
    }

    #[test]
    fn a_pixel_size_the_sidecar_left_out_has_no_card() {
        // `Optics::from_inputs` leaves a pixel size of 0 out, and the
        // header follows the sidecar rather than the driver's read.
        let optics = Optics::from_inputs(crate::persistence::document::OpticsInputs {
            focal_length_mm: Some(530.0),
            pixel_size_x_um: Some(0.0),
            pixel_size_y_um: Some(3.76),
            ..Default::default()
        })
        .unwrap();
        let cards = cards(&with_optics(optics));
        assert_eq!(value_of(&cards, "XPIXSZ"), None);
        assert_eq!(value_of(&cards, "YPIXSZ"), Some(KeywordValue::Float(7.52)));
        assert_eq!(
            value_of(&cards, "FOCALLEN"),
            Some(KeywordValue::Float(530.0))
        );
    }

    #[test]
    fn a_document_without_optics_site_or_version_carries_none_of_their_cards() {
        let mut doc = full_document();
        doc.optics = None;
        doc.site = None;
        doc.rp_version = None;
        let cards = cards(&doc);
        for key in [
            "XPIXSZ", "YPIXSZ", "FOCALLEN", "APTDIA", "FOCRATIO", "SITELAT", "SITELONG", "SWCREATE",
        ] {
            assert_eq!(value_of(&cards, key), None, "{key}");
        }
    }

    #[test]
    fn a_reserved_calibration_target_carries_no_object_cards() {
        let mut doc = full_document();
        doc.frame_type = Some(FrameType::Dark);
        doc.target = Some(ExposureTarget {
            slug: "dark".to_string(),
            display_name: None,
            ra_hours: None,
            dec_degrees: None,
        });
        let cards = cards(&doc);
        assert_eq!(
            value_of(&cards, "IMAGETYP"),
            Some(KeywordValue::Str("Dark Frame".into()))
        );
        for key in ["OBJECT", "OBJCTRA", "OBJCTDEC"] {
            assert_eq!(value_of(&cards, key), None, "{key}");
        }
    }

    #[test]
    fn free_text_folds_typographic_punctuation_to_ascii() {
        let mut doc = full_document();
        doc.target.as_mut().unwrap().display_name = Some("Pleiades \u{2013} M45".to_string());
        let cards = cards(&doc);
        assert_eq!(
            value_of(&cards, "OBJECT"),
            Some(KeywordValue::Str("Pleiades - M45".into()))
        );
    }

    #[test]
    fn a_value_longer_than_one_card_drops_only_its_card() {
        // 67 characters plus an apostrophe that doubles on the card is 69.
        for name in ["x".repeat(69), format!("{}'s", "x".repeat(67))] {
            let mut doc = full_document();
            doc.camera_name = Some(name.clone());
            let cards = cards(&doc);
            assert_eq!(value_of(&cards, "INSTRUME"), None, "{name:?}");
            assert_eq!(
                value_of(&cards, "OBJECT"),
                Some(KeywordValue::Str("M31".into()))
            );
        }
    }

    #[test]
    fn a_value_that_fills_the_card_exactly_is_kept() {
        for name in ["x".repeat(68), format!("{}'", "x".repeat(66))] {
            let mut doc = full_document();
            doc.camera_name = Some(name.clone());
            let cards = cards(&doc);
            assert_eq!(
                value_of(&cards, "INSTRUME"),
                Some(KeywordValue::Str(name.clone())),
                "{name:?}"
            );
        }
    }

    #[test]
    fn a_value_no_ascii_stand_in_keeps_distinct_drops_only_its_card() {
        let mut doc = full_document();
        doc.filter = Some("H\u{3b1}".to_string());
        let cards = cards(&doc);
        assert_eq!(value_of(&cards, "FILTER"), None);
        assert_eq!(
            value_of(&cards, "OBJECT"),
            Some(KeywordValue::Str("M31".into()))
        );
    }

    #[test]
    fn a_pasted_invisible_character_does_not_cost_the_card() {
        let mut doc = full_document();
        doc.target.as_mut().unwrap().display_name = Some("M31\u{200B}".to_string());
        let cards = cards(&doc);
        assert_eq!(
            value_of(&cards, "OBJECT"),
            Some(KeywordValue::Str("M31".into()))
        );
    }

    #[test]
    fn an_unparseable_start_time_omits_date_obs() {
        let mut doc = full_document();
        doc.exposure_started_at = Some("yesterday".to_string());
        let cards = cards(&doc);
        assert_eq!(value_of(&cards, "DATE-OBS"), None);
    }

    #[test]
    fn date_obs_is_utc_whatever_the_offset() {
        assert_eq!(
            fits_timestamp("2026-03-02T03:15:00.412+02:00").as_deref(),
            Some("2026-03-02T01:15:00.412")
        );
    }

    #[test]
    fn document_timestamp_round_trips_through_date_obs_at_millisecond_precision() {
        let at = DateTime::parse_from_rfc3339("2026-03-02T01:15:00.412987654Z")
            .unwrap()
            .with_timezone(&Utc);
        let stored = document_timestamp(at);
        assert_eq!(stored, "2026-03-02T01:15:00.412+00:00");
        assert_eq!(
            fits_timestamp(&stored).as_deref(),
            Some("2026-03-02T01:15:00.412")
        );
    }

    #[test]
    fn image_type_uses_the_maxim_spelling() {
        assert_eq!(image_type(FrameType::Light), "Light Frame");
        assert_eq!(image_type(FrameType::Dark), "Dark Frame");
        assert_eq!(image_type(FrameType::Flat), "Flat Field");
        assert_eq!(image_type(FrameType::Bias), "Bias Frame");
    }

    #[test]
    fn sexagesimal_ra_formats_and_carries() {
        assert_eq!(sexagesimal_ra(0.7123).as_deref(), Some("00 42 44.28"));
        assert_eq!(sexagesimal_ra(0.0).as_deref(), Some("00 00 00.00"));
        assert_eq!(sexagesimal_ra(23.5).as_deref(), Some("23 30 00.00"));
        // 59.999 s of the first minute rounds into the next one.
        assert_eq!(
            sexagesimal_ra(59.999 / 3600.0).as_deref(),
            Some("00 01 00.00")
        );
        // A hair under 24h wraps to 0h rather than printing 24.
        assert_eq!(sexagesimal_ra(23.999_999_9).as_deref(), Some("00 00 00.00"));
        assert_eq!(sexagesimal_ra(-1.0).as_deref(), Some("23 00 00.00"));
        assert_eq!(sexagesimal_ra(f64::NAN), None);
    }

    #[test]
    fn sexagesimal_dec_formats_sign_and_carries() {
        assert_eq!(sexagesimal_dec(41.2689).as_deref(), Some("+41 16 08.0"));
        assert_eq!(sexagesimal_dec(-5.391).as_deref(), Some("-05 23 27.6"));
        assert_eq!(sexagesimal_dec(90.0).as_deref(), Some("+90 00 00.0"));
        assert_eq!(sexagesimal_dec(-0.000_001).as_deref(), Some("+00 00 00.0"));
        assert_eq!(
            sexagesimal_dec(10.0 + 59.99 / 3600.0).as_deref(),
            Some("+10 01 00.0")
        );
        assert_eq!(sexagesimal_dec(90.5), None);
        assert_eq!(sexagesimal_dec(f64::INFINITY), None);
    }
}

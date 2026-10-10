//! The standard FITS keywords `capture` writes into each frame's primary
//! header — `docs/services/rp.md` §"FITS header".
//!
//! The header is a portable copy of the exposure document: every card
//! below is projected from an [`ExposureDocument`] field, or from rig
//! configuration the document does not repeat ([`HeaderContext`]). The
//! sidecar stays the authority. A card whose source is absent is left
//! out, never written as a placeholder, and a value `rp-fits` refuses
//! (a non-ASCII string, one too long for a single card) drops only that
//! card — no header problem fails a capture. `DOC_ID` is not built
//! here: [`super::fits`] stamps it on every write.

use chrono::{DateTime, SecondsFormat, Utc};
use rp_fits::writer::{Keyword, KeywordValue};
use rp_vocabulary::FrameType;
use tracing::debug;

use super::document::ExposureDocument;

/// `SWCREATE`: the software that wrote the frame.
const SOFTWARE: &str = concat!("rusty-photon rp ", env!("CARGO_PKG_VERSION"));

/// The FITS form of `DATE-OBS` (UTC, millisecond precision, no zone).
const FITS_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f";

/// What the header needs beyond the document: rig facts the sidecar
/// either does not carry or carries only inside the all-or-nothing
/// `optics` block, which the header does not wait on.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeaderContext<'a> {
    /// Unbinned `PixelSizeX`, µm, from the connect-time cache.
    pub pixel_size_x_um: Option<f64>,
    /// Unbinned `PixelSizeY`, µm.
    pub pixel_size_y_um: Option<f64>,
    /// The camera's train focal length, mm.
    pub focal_length_mm: Option<f64>,
    /// The configured observer site.
    pub site: Option<&'a rp_ephemeris::Site>,
}

/// Format an exposure-start instant for the document's
/// `exposure_started_at`: RFC 3339 at millisecond precision, the same
/// precision `DATE-OBS` carries.
#[must_use]
pub fn document_timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, false)
}

/// The standard keyword cards for one frame, in header order.
#[must_use]
pub fn header_keywords(doc: &ExposureDocument, ctx: &HeaderContext<'_>) -> Vec<Keyword> {
    let mut cards = Cards::default();

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
        cards.real(
            "RA",
            Some(pointing.ra_hours * 15.0),
            "[deg] mount right ascension",
        );
        cards.real("DEC", Some(pointing.dec_degrees), "[deg] mount declination");
    }
    cards.string("INSTRUME", doc.camera_name.as_deref(), "camera");
    cards.string("TELESCOP", doc.train_id.as_deref(), "optical train");
    cards.string("FILTER", doc.filter.as_deref(), "filter");
    cards.integer("GAIN", doc.gain, "camera gain");
    cards.integer("OFFSET", doc.offset, "camera offset");
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
    if let Some(binning) = doc.binning {
        cards.integer("XBINNING", Some(binning.x), "binning factor");
        cards.integer("YBINNING", Some(binning.y), "binning factor");
        // The binned pixel — the MaxIm convention plate solvers read
        // `XPIXSZ` by (rp.md §"FITS header").
        cards.real(
            "XPIXSZ",
            ctx.pixel_size_x_um.map(|um| um * f64::from(binning.x)),
            "[um] binned pixel width",
        );
        cards.real(
            "YPIXSZ",
            ctx.pixel_size_y_um.map(|um| um * f64::from(binning.y)),
            "[um] binned pixel height",
        );
    }
    cards.real("FOCALLEN", ctx.focal_length_mm, "[mm] focal length");
    if let Some(site) = ctx.site {
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
    cards.string("SWCREATE", Some(SOFTWARE), "creating software");

    cards.0
}

/// The accumulating card list. Each `push` validates through
/// [`Keyword::new`] and drops — with a `debug!` — a card `rp-fits`
/// refuses.
#[derive(Default)]
struct Cards(Vec<Keyword>);

impl Cards {
    fn push(&mut self, key: &'static str, value: KeywordValue, comment: &'static str) {
        match Keyword::new(key, value) {
            Ok(card) => self.0.push(card.with_comment(comment)),
            Err(e) => debug!(keyword = key, error = %e, "dropping FITS header card"),
        }
    }

    fn string(&mut self, key: &'static str, value: Option<&str>, comment: &'static str) {
        if let Some(value) = value {
            self.push(key, KeywordValue::Str(value.to_string()), comment);
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
    use crate::persistence::document::{ExposureTarget, MountPointing};

    /// The card list as `(key, value)` pairs, keys trimmed.
    fn cards(doc: &ExposureDocument, ctx: &HeaderContext<'_>) -> Vec<(String, KeywordValue)> {
        let mut buf = Vec::new();
        rp_fits::writer::write_u8_image(&mut buf, &[0], 1, 1, &header_keywords(doc, ctx)).unwrap();
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
            duration: Some(Duration::from_secs(300)),
            binning: Some(rp_vocabulary::Binning { x: 2, y: 2 }),
            filter: Some("Luminance".to_string()),
            gain: Some(26),
            offset: Some(30),
            max_adu: Some(65535),
            cooler_setpoint_c: Some(-10),
            sensor_temperature_c: Some(-9.8),
            pointing: Some(MountPointing {
                ra_hours: 0.7121,
                dec_degrees: 41.2702,
            }),
            optics: None,
            target: Some(ExposureTarget {
                slug: "m31".to_string(),
                display_name: Some("M31".to_string()),
                ra_hours: Some(0.7123),
                dec_degrees: Some(41.2689),
            }),
            frame_type: Some(FrameType::Light),
            sections: serde_json::Map::new(),
        }
    }

    fn full_context(site: &rp_ephemeris::Site) -> HeaderContext<'_> {
        HeaderContext {
            pixel_size_x_um: Some(3.76),
            pixel_size_y_um: Some(3.76),
            focal_length_mm: Some(1000.0),
            site: Some(site),
        }
    }

    #[test]
    fn a_full_document_yields_every_keyword_in_header_order() {
        let site = rp_ephemeris::Site::new(47.6062, -122.3321).unwrap();
        let cards = cards(&full_document(), &full_context(&site));
        assert_eq!(
            keys(&cards),
            [
                "DATE-OBS", "EXPTIME", "IMAGETYP", "OBJECT", "OBJCTRA", "OBJCTDEC", "RA", "DEC",
                "INSTRUME", "TELESCOP", "FILTER", "GAIN", "OFFSET", "CCD-TEMP", "SET-TEMP",
                "XBINNING", "YBINNING", "XPIXSZ", "YPIXSZ", "FOCALLEN", "SITELAT", "SITELONG",
                "SWCREATE",
            ]
        );
    }

    #[test]
    fn values_mirror_the_document() {
        let site = rp_ephemeris::Site::new(47.6062, -122.3321).unwrap();
        let cards = cards(&full_document(), &full_context(&site));
        let expect = [
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
            ("TELESCOP", KeywordValue::Str("main".into())),
            ("FILTER", KeywordValue::Str("Luminance".into())),
            ("GAIN", KeywordValue::Int(26)),
            ("OFFSET", KeywordValue::Int(30)),
            ("CCD-TEMP", KeywordValue::Float(-9.8)),
            ("SET-TEMP", KeywordValue::Float(-10.0)),
            ("XBINNING", KeywordValue::Int(2)),
            ("YBINNING", KeywordValue::Int(2)),
            ("FOCALLEN", KeywordValue::Float(1000.0)),
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
        let cards = cards(&full_document(), &HeaderContext::default());
        let ra = value_of(&cards, "RA").and_then(|v| v.as_real()).unwrap();
        let dec = value_of(&cards, "DEC").and_then(|v| v.as_real()).unwrap();
        assert!((ra - 0.7121 * 15.0).abs() < 1e-9, "RA {ra}");
        assert!((dec - 41.2702).abs() < 1e-9, "DEC {dec}");
    }

    #[test]
    fn pixel_size_is_scaled_by_the_binning() {
        let cards = cards(
            &full_document(),
            &HeaderContext {
                pixel_size_x_um: Some(3.76),
                pixel_size_y_um: Some(2.0),
                ..HeaderContext::default()
            },
        );
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
    fn a_bare_document_yields_only_the_software_card() {
        let cards = cards(&ExposureDocument::default(), &HeaderContext::default());
        assert_eq!(keys(&cards), ["SWCREATE"]);
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
        let cards = cards(&doc, &HeaderContext::default());
        assert_eq!(
            value_of(&cards, "IMAGETYP"),
            Some(KeywordValue::Str("Dark Frame".into()))
        );
        for key in ["OBJECT", "OBJCTRA", "OBJCTDEC"] {
            assert_eq!(value_of(&cards, key), None, "{key}");
        }
    }

    #[test]
    fn a_value_rp_fits_refuses_drops_only_its_card() {
        let mut doc = full_document();
        doc.target.as_mut().unwrap().display_name = Some("Pleiades \u{2013} M45".to_string());
        doc.camera_name = Some("x".repeat(80));
        let cards = cards(&doc, &HeaderContext::default());
        assert_eq!(value_of(&cards, "OBJECT"), None);
        assert_eq!(value_of(&cards, "INSTRUME"), None);
        assert_eq!(
            value_of(&cards, "OBJCTRA"),
            Some(KeywordValue::Str("00 42 44.28".into()))
        );
    }

    #[test]
    fn an_unparseable_start_time_omits_date_obs() {
        let mut doc = full_document();
        doc.exposure_started_at = Some("yesterday".to_string());
        let cards = cards(&doc, &HeaderContext::default());
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

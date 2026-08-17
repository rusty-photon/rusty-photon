//! ASTAP `.wcs` sidecar parser.
//!
//! ASTAP writes a `.wcs` sidecar next to each successfully solved FITS,
//! containing the World Coordinate System keywords as a FITS primary
//! HDU header (no data block — `NAXIS = 0` or all `NAXISn = 0`). Parsing
//! goes through [`fitsrs`] for the FITS-card layer and
//! [`wcs::WCSParams`] for the keyword-to-field mapping. The wrapper
//! accepts either CDELT/CROTA or CD-matrix WCS conventions for the
//! pixel-scale and rotation response fields; CRVAL1/CRVAL2 are always
//! required.
//!
//! The only divergence from a vanilla `Fits::from_reader` call is a
//! defensive pre-processor that pads short inputs out to the next
//! 2880-byte FITS block boundary. ASTAP's own output is already padded;
//! the pre-processor exists so test fixtures whose card stream stops
//! exactly at the END card still parse without a separate code path.
//! It does **not** lower the WCS-keyword bar — the parser still requires
//! a complete FITS primary HDU header (`SIMPLE`, `BITPIX`, `NAXIS`,
//! `CTYPE1`/`CTYPE2`, plus the WCS solution).

use super::{SolveOutcome, WcsMatrix};
use fitsrs::card::{Card, Value};
use fitsrs::hdu::header::{Header, Xtension};
use fitsrs::hdu::HDU;
use fitsrs::Fits;
use serde::de::IntoDeserializer;
use serde::Deserialize;
use std::io::Cursor;
use std::path::Path;
use thiserror::Error;
use wcs::WCSParams;

const FITS_BLOCK: usize = 2880;

#[derive(Debug, Error)]
pub enum WcsParseError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("required keyword `{0}` not found in .wcs sidecar")]
    MissingKeyword(&'static str),

    /// The card's value is unusable as a float: a non-numeric FITS type
    /// (logical, string, undefined), a non-finite float, or an integer
    /// beyond i32 range (WCS quantities are small reals; a larger
    /// integer is bogus data, not a value to round).
    #[error("`{key}` is non-numeric: {value}")]
    NonNumeric { key: &'static str, value: String },

    #[error("malformed FITS header in .wcs sidecar: {0}")]
    Malformed(String),
}

/// Parse a `.wcs` sidecar file and return the scalar fields the HTTP
/// contract surfaces, a solver banner string read from the file's
/// HISTORY / COMMENT cards, and — when the sidecar carries a complete
/// CRPIX + CD set — the full WCS linear mapping.
pub fn read_wcs_sidecar(path: &Path) -> Result<SolveOutcome, WcsParseError> {
    let bytes = std::fs::read(path)?;
    parse_wcs_bytes(&bytes)
}

/// Pure-function variant for unit testing.
pub fn parse_wcs_bytes(bytes: &[u8]) -> Result<SolveOutcome, WcsParseError> {
    let normalized = pad_to_fits_block(bytes);
    let mut hdu_list = Fits::from_reader(Cursor::new(&*normalized));
    let hdu = hdu_list
        .next()
        .ok_or_else(|| WcsParseError::Malformed("empty header".into()))?
        .map_err(|e| WcsParseError::Malformed(format!("primary HDU parse failed: {e}")))?;

    // fitsrs's `Fits::next()` always produces `HDU::Primary` on the
    // first iteration (other variants only come from `new_xtension`,
    // which fires from the second iteration onward), so the else arm
    // is API-unreachable — surface it as `Malformed` rather than
    // panicking should the upstream contract ever change.
    let HDU::Primary(primary) = hdu else {
        return Err(WcsParseError::Malformed(
            "expected primary HDU on first iteration".into(),
        ));
    };
    let header = primary.get_header();

    // Pre-extract the contract-required pointing fields directly off
    // the parsed header so type errors on CRVAL1/CRVAL2 surface as
    // named `NonNumeric { key, value }` rather than the generic serde
    // error `WCSParams::deserialize` would emit.
    let crval1 = read_required_float(header, "CRVAL1")?;
    let crval2 = read_required_float(header, "CRVAL2")?;

    // Pixel scale and rotation: prefer CDELT1/CROTA2 (the convention
    // ASTAP writes today), but fall back to deriving them from the
    // CD matrix when present. Either representation alone is
    // sufficient for these two response fields.
    let pixel_scale_arcsec = derive_pixel_scale_arcsec(header)?;
    let rotation_deg = derive_rotation_deg(header)?;

    // Full CRPIX + CD matrix, surfaced verbatim when complete.
    let wcs_matrix = read_wcs_matrix(header)?;

    // Run `WCSParams::deserialize` as the canonical structural validator
    // — catches missing CTYPE1, missing NAXIS, type errors on any other
    // WCS keyword (SIP, etc.) before we hand the wrapper a `SolveOutcome`
    // derived from a half-broken header. Forward-compatible with future
    // consumers that need the full `WCSParams`.
    WCSParams::deserialize(header.into_deserializer())
        .map_err(|e| WcsParseError::Malformed(format!("WCS deserialization failed: {e}")))?;

    let solver = find_banner(header).unwrap_or_else(|| "astap-cli".to_string());

    Ok(SolveOutcome {
        ra_center: crval1,
        dec_center: crval2,
        pixel_scale_arcsec,
        rotation_deg,
        wcs_matrix,
        solver,
    })
}

/// Full WCS linear mapping, all-or-nothing: `Some` only when the
/// header carries CRPIX1, CRPIX2 and the complete 2×2 CD matrix.
/// Never synthesized from CDELT/CROTA2 — a synthesized matrix would
/// fabricate parity (the CD determinant's sign). A present-but-
/// non-numeric key is still a named [`WcsParseError::NonNumeric`].
fn read_wcs_matrix<X>(header: &Header<X>) -> Result<Option<WcsMatrix>, WcsParseError>
where
    X: Xtension + std::fmt::Debug,
{
    let keys = [
        read_optional_float(header, "CRPIX1")?,
        read_optional_float(header, "CRPIX2")?,
        read_optional_float(header, "CD1_1")?,
        read_optional_float(header, "CD1_2")?,
        read_optional_float(header, "CD2_1")?,
        read_optional_float(header, "CD2_2")?,
    ];
    match keys {
        [Some(crpix1), Some(crpix2), Some(cd1_1), Some(cd1_2), Some(cd2_1), Some(cd2_2)] => {
            Ok(Some(WcsMatrix {
                crpix1,
                crpix2,
                cd1_1,
                cd1_2,
                cd2_1,
                cd2_2,
            }))
        }
        _ => Ok(None),
    }
}

/// Pixel scale in arcsec. Prefers `|CDELT1| × 3600`; if `CDELT1` is
/// absent, falls back to `√(CD1_1² + CD2_1²) × 3600` per the FITS WCS
/// CD-matrix convention. Returns `MissingKeyword("CDELT1")` only when
/// neither representation is present.
fn derive_pixel_scale_arcsec<X>(header: &Header<X>) -> Result<f64, WcsParseError>
where
    X: Xtension + std::fmt::Debug,
{
    if let Some(cdelt1) = read_optional_float(header, "CDELT1")? {
        return Ok(cdelt1.abs() * 3600.0);
    }
    if let (Some(cd1_1), Some(cd2_1)) = (
        read_optional_float(header, "CD1_1")?,
        read_optional_float(header, "CD2_1")?,
    ) {
        return Ok(cd1_1.hypot(cd2_1) * 3600.0);
    }
    Err(WcsParseError::MissingKeyword("CDELT1"))
}

/// Rotation in degrees. Prefers `CROTA2`; if absent, falls back to
/// `atan2(CD2_1, CD1_1)` (FITS WCS CD-matrix convention). Defaults to
/// 0 when neither representation is present (rotation is optional in
/// the HTTP contract).
fn derive_rotation_deg<X>(header: &Header<X>) -> Result<f64, WcsParseError>
where
    X: Xtension + std::fmt::Debug,
{
    if let Some(crota2) = read_optional_float(header, "CROTA2")? {
        return Ok(crota2);
    }
    if let (Some(cd1_1), Some(cd2_1)) = (
        read_optional_float(header, "CD1_1")?,
        read_optional_float(header, "CD2_1")?,
    ) {
        return Ok(cd2_1.atan2(cd1_1).to_degrees());
    }
    Ok(0.0)
}

/// Pad `bytes` (zero-copy when already aligned) to the next 2880-byte
/// FITS block boundary by appending ASCII spaces. Real ASTAP output is
/// already aligned; the pad keeps test fixtures and minimal sidecars
/// parseable without a divergent code path.
fn pad_to_fits_block(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let len = bytes.len();
    let remainder = len % FITS_BLOCK;
    if remainder == 0 && len > 0 {
        return std::borrow::Cow::Borrowed(bytes);
    }
    let target = if len == 0 {
        FITS_BLOCK
    } else {
        len.saturating_add(FITS_BLOCK.saturating_sub(remainder))
    };
    let mut padded = Vec::with_capacity(target);
    padded.extend_from_slice(bytes);
    padded.resize(target, b' ');
    std::borrow::Cow::Owned(padded)
}

/// Walk the header's cards looking for a HISTORY or COMMENT mentioning
/// "ASTAP". The banner is informational; the HTTP contract falls back
/// to a default when none is found.
fn find_banner<X>(header: &Header<X>) -> Option<String>
where
    X: Xtension + std::fmt::Debug,
{
    for card in header.cards() {
        let text = match card {
            Card::History(s) | Card::Comment(s) => s.as_str(),
            _ => continue,
        };
        if text.to_ascii_uppercase().contains("ASTAP") {
            return Some(text.trim().to_string());
        }
    }
    None
}

/// Read a contract-required numeric keyword. Returns
/// [`WcsParseError::MissingKeyword`] when the key is absent and
/// [`WcsParseError::NonNumeric`] when the card carries a non-numeric
/// value (string, logical, undefined, invalid).
fn read_required_float<X>(header: &Header<X>, key: &'static str) -> Result<f64, WcsParseError>
where
    X: Xtension + std::fmt::Debug,
{
    header
        .get(key)
        .ok_or(WcsParseError::MissingKeyword(key))
        .and_then(|value| coerce_float(key, value))
}

/// Read an optional numeric keyword. `None` when absent. Returns
/// [`WcsParseError::NonNumeric`] when the card is present but carries a
/// non-numeric value.
fn read_optional_float<X>(
    header: &Header<X>,
    key: &'static str,
) -> Result<Option<f64>, WcsParseError>
where
    X: Xtension + std::fmt::Debug,
{
    header
        .get(key)
        .map_or(Ok(None), |value| coerce_float(key, value).map(Some))
}

fn coerce_float(key: &'static str, value: &Value) -> Result<f64, WcsParseError> {
    match value {
        Value::Float { value, .. } if value.is_finite() => Ok(*value),
        // WCS quantities are small reals; an integer card beyond i32 (the
        // widest type with an exact `From` into f64 here) is bogus data,
        // not a value to round.
        Value::Integer { value, .. } => {
            i32::try_from(*value)
                .map(f64::from)
                .map_err(|_| WcsParseError::NonNumeric {
                    key,
                    value: value.to_string(),
                })
        }
        Value::Float { value, .. } => Err(WcsParseError::NonNumeric {
            key,
            value: value.to_string(),
        }),
        Value::String { value, .. } => Err(WcsParseError::NonNumeric {
            key,
            value: value.clone(),
        }),
        Value::Logical { value, .. } => Err(WcsParseError::NonNumeric {
            key,
            value: value.to_string(),
        }),
        Value::Invalid(s) => Err(WcsParseError::NonNumeric {
            key,
            value: s.clone(),
        }),
        Value::Undefined => Err(WcsParseError::NonNumeric {
            key,
            value: "undefined".to_string(),
        }),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// Build a complete FITS primary HDU `.wcs` byte stream from a list
    /// of `(keyword, value)` pairs. Always prepends `SIMPLE = T`,
    /// `BITPIX = 16`, `NAXIS = 2`, `NAXIS1 = 1024`, `NAXIS2 = 1024`,
    /// `CTYPE1 = 'RA---TAN'`, `CTYPE2 = 'DEC--TAN'` so [`WCSParams`]'s
    /// mandatory fields are present. Caller's pairs append after the
    /// preamble; pad to a 2880-byte FITS block.
    fn build_wcs(pairs: &[(&str, &str)]) -> Vec<u8> {
        let mut cards: Vec<String> = vec![
            card("SIMPLE", "T"),
            card("BITPIX", "16"),
            card("NAXIS", "2"),
            card("NAXIS1", "1024"),
            card("NAXIS2", "1024"),
            card("CTYPE1", "'RA---TAN'"),
            card("CTYPE2", "'DEC--TAN'"),
        ];
        for (k, v) in pairs {
            cards.push(card(k, v));
        }
        cards.push(format!("{:<80}", "END"));
        let mut out: Vec<u8> = cards.into_iter().flat_map(String::into_bytes).collect();
        let pad = FITS_BLOCK - (out.len() % FITS_BLOCK);
        if pad < FITS_BLOCK {
            out.resize(out.len() + pad, b' ');
        }
        out
    }

    fn card(key: &str, value: &str) -> String {
        let body = if value.starts_with('\'') {
            format!("{key:<8}= {value}")
        } else {
            format!("{key:<8}= {value:>20}")
        };
        format!("{body:<80}")
    }

    #[test]
    fn happy_path_extracts_four_fields() {
        let bytes = build_wcs(&[
            ("CRVAL1", "10.6848"),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
            ("CDELT2", "0.000291667"),
            ("CROTA2", "12.3"),
        ]);
        let out = parse_wcs_bytes(&bytes).unwrap();
        assert!((out.ra_center - 10.6848).abs() < 1e-6);
        assert!((out.dec_center - 41.2690).abs() < 1e-6);
        // |CDELT1| * 3600 = pixel scale in arcsec
        assert!((out.pixel_scale_arcsec - 1.05).abs() < 1e-3);
        assert!((out.rotation_deg - 12.3).abs() < 1e-6);
        // CDELT/CROTA alone must never synthesize a matrix — a
        // synthesized one would fabricate parity.
        assert_eq!(out.wcs_matrix, None);
    }

    #[test]
    fn complete_crpix_and_cd_set_populates_wcs_matrix() {
        let bytes = build_wcs(&[
            ("CRVAL1", "10.6848"),
            ("CRVAL2", "41.2690"),
            ("CRPIX1", "512.0"),
            ("CRPIX2", "384.0"),
            ("CD1_1", "-0.000291"),
            ("CD1_2", "0.0000012"),
            ("CD2_1", "0.0000011"),
            ("CD2_2", "0.000291"),
        ]);
        let out = parse_wcs_bytes(&bytes).unwrap();
        let matrix = out.wcs_matrix.unwrap();
        assert_eq!(matrix.crpix1, 512.0);
        assert_eq!(matrix.crpix2, 384.0);
        assert_eq!(matrix.cd1_1, -0.000_291);
        assert_eq!(matrix.cd1_2, 0.000_001_2);
        assert_eq!(matrix.cd2_1, 0.000_001_1);
        assert_eq!(matrix.cd2_2, 0.000_291);
    }

    #[test]
    fn cd_without_crpix_leaves_wcs_matrix_none() {
        // Complete CD matrix, no CRPIX: scalar fields derive from the
        // CD matrix as before, but the all-or-nothing matrix stays
        // absent.
        let bytes = build_wcs(&[
            ("CRVAL1", "10.6848"),
            ("CRVAL2", "41.2690"),
            ("CD1_1", "-0.000291667"),
            ("CD1_2", "0.0"),
            ("CD2_1", "0.0"),
            ("CD2_2", "0.000291667"),
        ]);
        let out = parse_wcs_bytes(&bytes).unwrap();
        assert_eq!(out.wcs_matrix, None);
        assert!((out.pixel_scale_arcsec - 1.05).abs() < 1e-3);
    }

    #[test]
    fn partial_cd_with_crpix_leaves_wcs_matrix_none() {
        // CRPIX present but only half the CD matrix (the same partial
        // set the scalar fallbacks accept): pixel scale and rotation
        // still derive, the matrix stays absent.
        let bytes = build_wcs(&[
            ("CRVAL1", "10.6848"),
            ("CRVAL2", "41.2690"),
            ("CRPIX1", "512.0"),
            ("CRPIX2", "384.0"),
            ("CD1_1", "-0.000291667"),
            ("CD2_1", "0.0"),
        ]);
        let out = parse_wcs_bytes(&bytes).unwrap();
        assert_eq!(out.wcs_matrix, None);
        assert!((out.pixel_scale_arcsec - 1.05).abs() < 1e-3);
        assert!(
            (out.rotation_deg.abs() - 180.0).abs() < 1e-6,
            "got rotation_deg={}",
            out.rotation_deg
        );
    }

    #[test]
    fn non_numeric_cd_key_returns_named_error() {
        // A present-but-broken matrix key must surface as NonNumeric,
        // not silently degrade to wcs_matrix = None.
        let bytes = build_wcs(&[
            ("CRVAL1", "10.6848"),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
            ("CRPIX1", "512.0"),
            ("CRPIX2", "384.0"),
            ("CD1_1", "'oops'"),
            ("CD1_2", "0.0"),
            ("CD2_1", "0.0"),
            ("CD2_2", "0.000291667"),
        ]);
        let err = parse_wcs_bytes(&bytes).unwrap_err();
        match err {
            WcsParseError::NonNumeric { key, .. } => assert_eq!(key, "CD1_1"),
            other => panic!("expected NonNumeric for string CD1_1, got {other:?}"),
        }
    }

    #[test]
    fn missing_crval1_is_named_error() {
        let bytes = build_wcs(&[("CRVAL2", "41.2690"), ("CDELT1", "-0.000291667")]);
        let err = parse_wcs_bytes(&bytes).unwrap_err();
        match err {
            WcsParseError::MissingKeyword(k) => assert_eq!(k, "CRVAL1"),
            other => panic!("expected MissingKeyword, got {other:?}"),
        }
    }

    #[test]
    fn missing_crval2_is_named_error() {
        let bytes = build_wcs(&[("CRVAL1", "10.6848"), ("CDELT1", "-0.000291667")]);
        let err = parse_wcs_bytes(&bytes).unwrap_err();
        match err {
            WcsParseError::MissingKeyword(k) => assert_eq!(k, "CRVAL2"),
            other => panic!("expected MissingKeyword, got {other:?}"),
        }
    }

    #[test]
    fn missing_cdelt1_is_named_error() {
        // No CDELT1 and no CD-matrix — both representations absent
        // surfaces as MissingKeyword("CDELT1") (the canonical name the
        // HTTP contract uses for pixel scale).
        let bytes = build_wcs(&[("CRVAL1", "10.6848"), ("CRVAL2", "41.2690")]);
        let err = parse_wcs_bytes(&bytes).unwrap_err();
        match err {
            WcsParseError::MissingKeyword(k) => assert_eq!(k, "CDELT1"),
            other => panic!("expected MissingKeyword, got {other:?}"),
        }
    }

    #[test]
    fn cd_matrix_drives_pixel_scale_when_cdelt1_absent() {
        // FITS WCS CD-matrix convention: pixel_scale = √(CD1_1² + CD2_1²)
        // along NAXIS1. With CD1_1 = -2.91667e-4 and CD2_1 = 0 (unrotated),
        // |CDELT1| = 2.91667e-4 deg ≈ 1.05 arcsec/pixel.
        let bytes = build_wcs(&[
            ("CRVAL1", "10.6848"),
            ("CRVAL2", "41.2690"),
            ("CD1_1", "-0.000291667"),
            ("CD1_2", "0.0"),
            ("CD2_1", "0.0"),
            ("CD2_2", "0.000291667"),
        ]);
        let out = parse_wcs_bytes(&bytes).unwrap();
        assert!(
            (out.pixel_scale_arcsec - 1.05).abs() < 1e-3,
            "got pixel_scale_arcsec={}",
            out.pixel_scale_arcsec
        );
        // Unrotated CD matrix: rotation falls out as atan2(0, CD1_1).
        // CD1_1 < 0 → atan2 returns π (180°) — that's the convention
        // for an x-axis flip, which RA-decreasing-rightward implies.
        assert!(
            (out.rotation_deg.abs() - 180.0).abs() < 1e-6 || out.rotation_deg.abs() < 1e-6,
            "got rotation_deg={}",
            out.rotation_deg
        );
    }

    #[test]
    fn cd_matrix_rotation_derives_when_crota2_absent() {
        // 30° rotated CD matrix: CD1_1 = s·cos(30°), CD2_1 = s·sin(30°).
        // atan2(CD2_1, CD1_1) = 30°.
        let s: f64 = 0.000_291_667; // pixel scale in deg
        let theta_deg = 30.0_f64;
        let cd1_1 = s * theta_deg.to_radians().cos();
        let cd2_1 = s * theta_deg.to_radians().sin();
        let bytes = build_wcs(&[
            ("CRVAL1", "10.6848"),
            ("CRVAL2", "41.2690"),
            ("CD1_1", &format!("{cd1_1:.10}")),
            ("CD2_1", &format!("{cd2_1:.10}")),
            ("CD1_2", &format!("{:.10}", -cd2_1)),
            ("CD2_2", &format!("{cd1_1:.10}")),
        ]);
        let out = parse_wcs_bytes(&bytes).unwrap();
        assert!(
            (out.rotation_deg - 30.0).abs() < 1e-3,
            "got rotation_deg={}",
            out.rotation_deg
        );
        assert!(
            (out.pixel_scale_arcsec - s.abs() * 3600.0).abs() < 1e-3,
            "got pixel_scale_arcsec={}",
            out.pixel_scale_arcsec
        );
    }

    #[test]
    fn non_numeric_crval1_returns_named_error() {
        // ASTAP would never emit this, but a corrupt or wrong-tooling
        // sidecar might land here. The wrapper must surface the bad
        // keyword by name rather than collapsing to a generic
        // deserialization failure (issue #160 review feedback).
        let bytes = build_wcs(&[
            ("CRVAL1", "'oops'"),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
        ]);
        let err = parse_wcs_bytes(&bytes).unwrap_err();
        match err {
            WcsParseError::NonNumeric { key, value } => {
                assert_eq!(key, "CRVAL1");
                assert!(
                    value.contains("oops"),
                    "value didn't include 'oops': {value}"
                );
            }
            other => panic!("expected NonNumeric, got {other:?}"),
        }
    }

    #[test]
    fn naxis_zero_header_only_sidecar_parses() {
        // Real ASTAP writes a `.wcs` whose primary HDU has no data block
        // (`NAXIS = 0`). Confirm the parser accepts that production
        // shape — issue #160 review feedback. `build_wcs` emits a
        // `NAXIS = 2` preamble for `WCSParams`'s mandatory keywords, so
        // construct this case by hand.
        let cards = [
            card("SIMPLE", "T"),
            card("BITPIX", "8"),
            card("NAXIS", "0"),
            card("CTYPE1", "'RA---TAN'"),
            card("CTYPE2", "'DEC--TAN'"),
            card("CRVAL1", "10.6848"),
            card("CRVAL2", "41.2690"),
            card("CDELT1", "-0.000291667"),
            card("CDELT2", "0.000291667"),
            card("CROTA2", "5.0"),
            format!("{:<80}", "END"),
        ];
        let mut bytes: Vec<u8> = cards.into_iter().flat_map(String::into_bytes).collect();
        let pad = FITS_BLOCK - (bytes.len() % FITS_BLOCK);
        if pad < FITS_BLOCK {
            bytes.resize(bytes.len() + pad, b' ');
        }
        let out = parse_wcs_bytes(&bytes).unwrap();
        assert!((out.ra_center - 10.6848).abs() < 1e-6);
        assert!((out.dec_center - 41.2690).abs() < 1e-6);
        assert!((out.rotation_deg - 5.0).abs() < 1e-6);
    }

    #[test]
    fn missing_crota2_defaults_to_zero() {
        let bytes = build_wcs(&[
            ("CRVAL1", "10.6848"),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
        ]);
        let out = parse_wcs_bytes(&bytes).unwrap();
        assert_eq!(out.rotation_deg, 0.0);
    }

    #[test]
    fn comment_card_with_astap_becomes_solver_banner() {
        let bytes = build_wcs(&[
            ("CRVAL1", "10.6848"),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
            // COMMENT cards have a free-form value column; build_wcs's
            // generic shape works because fitsrs treats anything past
            // the 8-byte keyword column as the comment text.
        ]);
        // Splice a COMMENT card before END.
        let mut spliced = bytes;
        // Find END card by walking 80-byte cards.
        let end_idx = (0..spliced.len())
            .step_by(80)
            .find(|i| &spliced[*i..*i + 8] == b"END     ")
            .expect("END card not found");
        let comment = format!("{:<80}", "COMMENT ASTAP solver version CLI-2026.04.28");
        spliced.splice(end_idx..end_idx, comment.into_bytes());
        // Re-pad: splicing pushed the END further; ensure the buffer is
        // still a 2880 multiple by padding more spaces if needed.
        let pad = FITS_BLOCK - (spliced.len() % FITS_BLOCK);
        if pad < FITS_BLOCK {
            spliced.resize(spliced.len() + pad, b' ');
        }
        let out = parse_wcs_bytes(&spliced).unwrap();
        assert!(out.solver.contains("ASTAP"), "got banner: {}", out.solver);
    }

    #[test]
    fn pixel_scale_uses_absolute_value_of_cdelt1() {
        // ASTAP writes negative CDELT1 because RA decreases left-to-right.
        let bytes = build_wcs(&[
            ("CRVAL1", "10.6848"),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
        ]);
        let out = parse_wcs_bytes(&bytes).unwrap();
        assert!(out.pixel_scale_arcsec > 0.0);
    }

    #[test]
    fn unpadded_input_is_padded_defensively() {
        // Build a header that ends right after the END card, with no
        // trailing 2880 padding. The pre-processor must pad it before
        // handing it to fitsrs.
        let cards = [
            card("SIMPLE", "T"),
            card("BITPIX", "16"),
            card("NAXIS", "2"),
            card("NAXIS1", "1024"),
            card("NAXIS2", "1024"),
            card("CTYPE1", "'RA---TAN'"),
            card("CTYPE2", "'DEC--TAN'"),
            card("CRVAL1", "10.6848"),
            card("CRVAL2", "41.2690"),
            card("CDELT1", "-0.000291667"),
            format!("{:<80}", "END"),
        ];
        let unpadded: Vec<u8> = cards.into_iter().flat_map(String::into_bytes).collect();
        assert_ne!(unpadded.len() % FITS_BLOCK, 0, "test setup precondition");
        let out = parse_wcs_bytes(&unpadded).unwrap();
        assert!((out.ra_center - 10.6848).abs() < 1e-6);
    }

    #[test]
    fn read_wcs_sidecar_reads_from_disk() {
        // Cover the public file-IO entry point. `parse_wcs_bytes` is
        // exercised by every other test; this one writes a fixture to a
        // temp dir and confirms `read_wcs_sidecar` reads + parses it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("solve.wcs");
        let bytes = build_wcs(&[
            ("CRVAL1", "10.6848"),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
        ]);
        std::fs::write(&path, bytes).unwrap();
        let out = read_wcs_sidecar(&path).unwrap();
        assert!((out.ra_center - 10.6848).abs() < 1e-6);
    }

    #[test]
    fn empty_input_returns_malformed() {
        let err = parse_wcs_bytes(&[]).unwrap_err();
        match err {
            WcsParseError::Malformed(_) => {}
            other => panic!("expected Malformed for empty input, got {other:?}"),
        }
    }

    #[test]
    fn integer_typed_crval1_is_coerced_to_float() {
        // FITS values without a decimal point parse as `Value::Integer`;
        // coerce_float must lift them into f64. Build a card with an
        // integer-shaped CRVAL1 value.
        let bytes = build_wcs(&[
            ("CRVAL1", "11"),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
        ]);
        let out = parse_wcs_bytes(&bytes).unwrap();
        assert!((out.ra_center - 11.0).abs() < 1e-9);
    }

    #[test]
    fn integer_crval1_beyond_i32_returns_non_numeric() {
        // Integer cards convert via i32::try_from + f64::from; a value
        // past i32::MAX (here i32::MAX + 1) is bogus for a WCS quantity
        // and must surface as NonNumeric instead of a lossy rounding.
        let bytes = build_wcs(&[
            ("CRVAL1", "2147483648"),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
        ]);
        let err = parse_wcs_bytes(&bytes).unwrap_err();
        match err {
            WcsParseError::NonNumeric { key, value } => {
                assert_eq!(key, "CRVAL1");
                assert_eq!(value, "2147483648");
            }
            other => panic!("expected NonNumeric for oversized CRVAL1, got {other:?}"),
        }
    }

    #[test]
    fn logical_crval1_returns_non_numeric() {
        // Bare `T` in the value column parses as `Value::Logical`; bad
        // input shape from a wrong-tooling sidecar must surface as a
        // named NonNumeric error rather than reach `WCSParams::deserialize`.
        let bytes = build_wcs(&[
            ("CRVAL1", "T"),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
        ]);
        let err = parse_wcs_bytes(&bytes).unwrap_err();
        match err {
            WcsParseError::NonNumeric { key, .. } => assert_eq!(key, "CRVAL1"),
            other => panic!("expected NonNumeric for logical CRVAL1, got {other:?}"),
        }
    }

    #[test]
    fn invalid_value_crval1_returns_non_numeric() {
        // A value column starting with a non-quote, non-T/F, non-digit
        // character lands fitsrs in `Value::Invalid` (FITSv4 4.1.2.3).
        // coerce_float must surface that as a named error.
        let bytes = build_wcs(&[
            ("CRVAL1", "abc"),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
        ]);
        let err = parse_wcs_bytes(&bytes).unwrap_err();
        match err {
            WcsParseError::NonNumeric { key, .. } => assert_eq!(key, "CRVAL1"),
            other => panic!("expected NonNumeric for invalid CRVAL1, got {other:?}"),
        }
    }

    #[test]
    fn undefined_value_crval1_returns_non_numeric() {
        // Empty value column parses as `Value::Undefined`; coerce_float
        // must surface that as a named error rather than a generic one.
        let bytes = build_wcs(&[
            ("CRVAL1", ""),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
        ]);
        let err = parse_wcs_bytes(&bytes).unwrap_err();
        match err {
            WcsParseError::NonNumeric { key, value } => {
                assert_eq!(key, "CRVAL1");
                assert_eq!(value, "undefined");
            }
            other => panic!("expected NonNumeric for empty CRVAL1, got {other:?}"),
        }
    }

    #[test]
    fn banner_falls_back_when_comment_lacks_astap() {
        // A COMMENT card present but unrelated to ASTAP must NOT become
        // the banner; the wrapper falls back to "astap-cli". Covers the
        // find_banner false branch (the if-condition's else path).
        let bytes = build_wcs(&[
            ("CRVAL1", "10.6848"),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
        ]);
        let mut spliced = bytes;
        let end_idx = (0..spliced.len())
            .step_by(80)
            .find(|i| &spliced[*i..*i + 8] == b"END     ")
            .expect("END card not found");
        let comment = format!("{:<80}", "COMMENT unrelated note about flat darks");
        spliced.splice(end_idx..end_idx, comment.into_bytes());
        let out = parse_wcs_bytes(&spliced).unwrap();
        assert_eq!(out.solver, "astap-cli");
    }

    #[test]
    fn non_finite_crval1_returns_non_numeric() {
        // Float overflow → infinity. coerce_float's finite guard catches
        // it before WCSParams::deserialize would silently accept Inf.
        let bytes = build_wcs(&[
            ("CRVAL1", "1e400"),
            ("CRVAL2", "41.2690"),
            ("CDELT1", "-0.000291667"),
        ]);
        let err = parse_wcs_bytes(&bytes).unwrap_err();
        match err {
            WcsParseError::NonNumeric { key, .. } => assert_eq!(key, "CRVAL1"),
            other => panic!("expected NonNumeric for non-finite CRVAL1, got {other:?}"),
        }
    }

    #[test]
    fn missing_simple_card_returns_malformed() {
        // Build a buffer that lacks SIMPLE = T at the top.
        let cards = [
            card("BITPIX", "16"),
            card("NAXIS", "0"),
            format!("{:<80}", "END"),
        ];
        let bytes: Vec<u8> = cards.into_iter().flat_map(String::into_bytes).collect();
        let err = parse_wcs_bytes(&bytes).unwrap_err();
        match err {
            WcsParseError::Malformed(_) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }
}

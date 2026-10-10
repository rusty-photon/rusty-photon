//! BDD step definitions for the standard keywords `capture` writes into
//! each frame's FITS header (`capture_fits_header.feature`, rp.md
//! § Persistence → FITS header).

use std::fs::File;
use std::io::BufReader;

use cucumber::gherkin::Step;
use cucumber::{given, then};
use rp_fits::writer::KeywordValue;
use serde_json::Value;

use bdd_infra::rp_harness::{MountConfig, OpticalTrainConfig};

use crate::steps::tool_steps::{
    add_camera, add_filter_wheel, ensure_mcp_client, ensure_omnisim, start_rp,
};
use crate::world::RpWorld;

/// The FITS form `DATE-OBS` is written in (rp.md § FITS header).
const FITS_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f";

// ---------------------------------------------------------------------------
// Given
// ---------------------------------------------------------------------------

/// Camera + filter wheel in train `main` (1000 mm), a mount, and a site
/// — every source the header draws on, so each keyword has something to
/// mirror. The site is the simulator mount's own: rp refuses to start
/// when the configured site disagrees with the mount's (rp.md § Site
/// Validation Against the ASCOM Mount).
#[given("rp is running with a fully equipped capture rig on the simulator")]
async fn rp_running_with_fully_equipped_rig(world: &mut RpWorld) {
    ensure_omnisim(world).await;
    add_camera(world);
    add_filter_wheel(world);
    let url = world.omnisim_url();
    world.mount = Some(MountConfig {
        alpaca_url: url,
        device_number: 0,
        settle_after_slew: None,
    });
    world.optical_trains.push(OpticalTrainConfig {
        aperture_mm: None,
        id: "main".to_string(),
        purpose: Some("imaging".to_string()),
        focal_length_mm: Some(1000.0),
        default_position_angle_degrees: None,
        devices: vec!["main-fw".to_string(), "main-cam".to_string()],
        auto_focus: None,
    });
    world.site = Some((51.0786, -0.2944));
    start_rp(world).await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn captured_fits_path(world: &RpWorld) -> String {
    world
        .last_image_path
        .clone()
        .expect("no captured image path — the capture step must succeed first")
}

/// One keyword from the captured frame's primary header; `None` when
/// the card is absent.
fn header_keyword(world: &RpWorld, key: &str) -> Option<KeywordValue> {
    let path = captured_fits_path(world);
    let file = File::open(&path).unwrap_or_else(|e| panic!("cannot open {path}: {e}"));
    rp_fits::reader::read_primary_keyword(BufReader::new(file), key)
        .unwrap_or_else(|e| panic!("cannot read {key} from {path}: {e}"))
}

fn header_real(world: &RpWorld, key: &str) -> f64 {
    match header_keyword(world, key) {
        Some(KeywordValue::Float(v)) => v,
        other => panic!("expected {key} to be a real card, got {other:?}"),
    }
}

fn header_string(world: &RpWorld, key: &str) -> String {
    match header_keyword(world, key) {
        Some(KeywordValue::Str(s)) => s,
        other => panic!("expected {key} to be a string card, got {other:?}"),
    }
}

/// Substitute the placeholders a value cell may use: `{version}` is
/// rp's own version, `{document_id}` the capture's document id.
fn expand_placeholders(world: &RpWorld, cell: &str) -> String {
    let mut value = cell.replace("{version}", env!("CARGO_PKG_VERSION"));
    if value.contains("{document_id}") {
        let id = world
            .last_document_id
            .as_deref()
            .expect("no captured document id — the capture step must succeed first");
        value = value.replace("{document_id}", id);
    }
    value
}

fn table_rows(step: &Step) -> Vec<Vec<String>> {
    step.table
        .as_ref()
        .expect("this step needs a data table")
        .rows
        .iter()
        .skip(1)
        .cloned()
        .collect()
}

const fn last_document(world: &RpWorld) -> &Value {
    world
        .last_document_response_body
        .as_ref()
        .expect("no document fetched — add an 'I fetch the document ...' step first")
}

fn document_timestamp(world: &RpWorld, field: &str) -> chrono::DateTime<chrono::Utc> {
    let doc = last_document(world);
    let raw = doc
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("document field '{field}' missing or not a string: {doc}"));
    chrono::DateTime::parse_from_rfc3339(raw)
        .unwrap_or_else(|e| panic!("document field '{field}' = {raw:?} is not RFC 3339: {e}"))
        .with_timezone(&chrono::Utc)
}

/// Smallest angle between two right ascensions, in degrees — the pair
/// wraps at 0/360.
fn ra_separation_deg(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

// ---------------------------------------------------------------------------
// Then
// ---------------------------------------------------------------------------

#[then("the captured FITS header should carry these keywords:")]
fn header_carries_keywords(world: &mut RpWorld, step: &Step) {
    for row in table_rows(step) {
        let [key, kind, cell] = row.as_slice() else {
            panic!("expected | keyword | type | value |, got {row:?}");
        };
        let expected = expand_placeholders(world, cell);
        let actual = header_keyword(world, key);
        match (kind.as_str(), actual) {
            ("string", Some(KeywordValue::Str(s))) => {
                assert_eq!(s, expected, "{key} mismatch");
            }
            ("integer", Some(KeywordValue::Int(n))) => {
                let want: i64 = expected
                    .parse()
                    .unwrap_or_else(|e| panic!("bad integer {expected:?} for {key}: {e}"));
                assert_eq!(n, want, "{key} mismatch");
            }
            ("real", Some(KeywordValue::Float(v))) => {
                let want: f64 = expected
                    .parse()
                    .unwrap_or_else(|e| panic!("bad real {expected:?} for {key}: {e}"));
                assert!(
                    (v - want).abs() <= 1e-9 * want.abs().max(1.0),
                    "{key} is {v}, expected {want}"
                );
            }
            (kind, actual) => panic!("expected {key} to be a {kind} card, got {actual:?}"),
        }
    }
}

#[then("the captured FITS header should not carry these keywords:")]
fn header_lacks_keywords(world: &mut RpWorld, step: &Step) {
    for row in table_rows(step) {
        let [key] = row.as_slice() else {
            panic!("expected | keyword |, got {row:?}");
        };
        let actual = header_keyword(world, key);
        assert!(actual.is_none(), "expected no {key} card, got {actual:?}");
    }
}

#[then(expr = "the captured FITS header {string} should be a UTC timestamp of the form {string}")]
fn header_is_fits_timestamp(world: &mut RpWorld, key: String, form: String) {
    assert_eq!(
        form, "YYYY-MM-DDThh:mm:ss.sss",
        "this step checks the FITS millisecond form only"
    );
    let value = header_string(world, &key);
    assert_eq!(
        value.len(),
        form.len(),
        "{key} = {value:?} is not of the form {form}"
    );
    chrono::NaiveDateTime::parse_from_str(&value, FITS_TIMESTAMP_FORMAT)
        .unwrap_or_else(|e| panic!("{key} = {value:?} is not of the form {form}: {e}"));
}

/// The mount does not hold still (an untracked simulator mount's RA
/// follows sidereal time), so rp's read and this one differ by however
/// long passed between them — hence a tolerance rather than equality
/// (testing.md §5.14).
#[then(
    expr = "the captured FITS header RA and DEC should match the mount's reported position within {float} degrees"
)]
async fn header_pointing_matches_mount(world: &mut RpWorld, tolerance_deg: f64) {
    let ra_deg = header_real(world, "RA");
    let dec_deg = header_real(world, "DEC");
    ensure_mcp_client(world).await;
    let position = world
        .mcp()
        .call_tool("get_mount_position", serde_json::json!({}))
        .await
        .unwrap_or_else(|e| panic!("get_mount_position failed: {e}"));
    let mount_ra_deg = position["ra"].as_f64().expect("ra in hours") * 15.0;
    let mount_dec_deg = position["dec"].as_f64().expect("dec in degrees");
    assert!(
        ra_separation_deg(ra_deg, mount_ra_deg) <= tolerance_deg,
        "header RA {ra_deg} deg vs mount {mount_ra_deg} deg"
    );
    assert!(
        (dec_deg - mount_dec_deg).abs() <= tolerance_deg,
        "header DEC {dec_deg} deg vs mount {mount_dec_deg} deg"
    );
}

#[then("the captured FITS header DATE-OBS should be the document's exposure_started_at")]
fn header_date_obs_mirrors_document(world: &mut RpWorld) {
    let started = document_timestamp(world, "exposure_started_at");
    let date_obs = header_string(world, "DATE-OBS");
    assert_eq!(
        date_obs,
        started.format(FITS_TIMESTAMP_FORMAT).to_string(),
        "DATE-OBS does not mirror exposure_started_at"
    );
}

#[then("the document's exposure_started_at should not be later than its captured_at")]
fn document_started_before_captured(world: &mut RpWorld) {
    let started = document_timestamp(world, "exposure_started_at");
    let captured = document_timestamp(world, "captured_at");
    assert!(
        started <= captured,
        "exposure_started_at {started} is after captured_at {captured}"
    );
}

#[then("the captured FITS header RA and DEC should be the document's pointing in degrees")]
fn header_pointing_mirrors_document(world: &mut RpWorld) {
    let doc = last_document(world);
    let pointing = doc
        .get("pointing")
        .unwrap_or_else(|| panic!("document has no pointing: {doc}"));
    let ra_hours = pointing["ra_hours"].as_f64().expect("pointing.ra_hours");
    let dec_degrees = pointing["dec_degrees"]
        .as_f64()
        .expect("pointing.dec_degrees");
    let ra_deg = header_real(world, "RA");
    let dec_deg = header_real(world, "DEC");
    assert!(
        (ra_deg - ra_hours * 15.0).abs() <= 1e-9,
        "RA {ra_deg} is not pointing.ra_hours {ra_hours} x 15"
    );
    assert!(
        (dec_deg - dec_degrees).abs() <= 1e-9,
        "DEC {dec_deg} is not pointing.dec_degrees {dec_degrees}"
    );
}

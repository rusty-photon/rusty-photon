//! BDD step definitions for the image HTTP API:
//! `GET /api/images/{document_id}` (metadata) and
//! `GET /api/images/{document_id}/pixels` (ASCOM Alpaca `ImageBytes`).
//!
//! Shared steps live in `tool_steps.rs` (capture sets `last_document_id`)
//! and in equipment / session step modules (Given the simulator and rp).

use cucumber::gherkin::Step;
use cucumber::{then, when};
use serde_json::Value;

use crate::world::RpWorld;

// --- When steps ---

#[when("I fetch the image metadata for the captured document_id")]
async fn fetch_image_metadata_captured(world: &mut RpWorld) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");
    fetch_image_metadata(world, &document_id).await;
}

#[when(expr = "I fetch the image metadata for document_id {string}")]
async fn fetch_image_metadata_explicit(world: &mut RpWorld, document_id: String) {
    fetch_image_metadata(world, &document_id).await;
}

#[when("I fetch the image pixels for the captured document_id")]
async fn fetch_image_pixels_captured(world: &mut RpWorld) {
    let document_id = world
        .last_document_id
        .clone()
        .expect("no captured document_id available");
    fetch_image_pixels(world, &document_id).await;
}

#[when(expr = "I fetch the image pixels for document_id {string}")]
async fn fetch_image_pixels_explicit(world: &mut RpWorld, document_id: String) {
    fetch_image_pixels(world, &document_id).await;
}

async fn fetch_image_metadata(world: &mut RpWorld, document_id: &str) {
    let url = format!("{}/api/images/{}", world.rp_url(), document_id);
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /api/images/{id}");
    world.last_image_metadata_status = Some(resp.status().as_u16());
    world.last_image_metadata = resp.json::<Value>().await.ok();
}

async fn fetch_image_pixels(world: &mut RpWorld, document_id: &str) {
    let url = format!("{}/api/images/{}/pixels", world.rp_url(), document_id);
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .send()
        .await
        .expect("failed to GET /api/images/{id}/pixels");
    world.last_image_pixels_status = Some(resp.status().as_u16());
    world.last_image_pixels_content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    world.last_image_pixels_body = resp.bytes().await.ok().map(|b| b.to_vec());
}

// --- Then steps: metadata ---

#[then(expr = "the image metadata response status should be {int}")]
fn metadata_status_eq(world: &mut RpWorld, expected: u16) {
    let status = world
        .last_image_metadata_status
        .expect("no image metadata response recorded");
    assert_eq!(status, expected, "unexpected metadata status");
}

#[then(expr = "the image metadata should contain {string}")]
fn metadata_contains_field(world: &mut RpWorld, field: String) {
    let body = metadata_or_panic(world);
    assert!(
        body.get(&field).is_some(),
        "expected '{field}' in image metadata, got: {body:?}"
    );
}

#[then(expr = "the image metadata should contain {string} as a positive integer")]
fn metadata_contains_positive_integer(world: &mut RpWorld, field: String) {
    let body = metadata_or_panic(world);
    let value = body
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in image metadata, got: {body:?}"));
    let num = value.as_u64().unwrap_or_else(|| {
        panic!("expected '{field}' to be a non-negative integer, got: {value:?}")
    });
    assert!(num > 0, "expected '{field}' to be positive, got: {num}");
}

#[then(expr = "the image metadata should contain {string} with value {int}")]
fn metadata_field_equals_int(world: &mut RpWorld, field: String, expected: i64) {
    let body = metadata_or_panic(world);
    let value = body
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in image metadata, got: {body:?}"));
    let actual = value
        .as_i64()
        .or_else(|| value.as_u64().map(u64::cast_signed))
        .unwrap_or_else(|| panic!("expected '{field}' to be an integer, got: {value:?}"));
    assert_eq!(actual, expected, "field '{field}'");
}

#[then(expr = "the image metadata should contain {string} with value true")]
fn metadata_field_is_true(world: &mut RpWorld, field: String) {
    let body = metadata_or_panic(world);
    let value = body
        .get(&field)
        .unwrap_or_else(|| panic!("expected '{field}' in image metadata, got: {body:?}"));
    assert_eq!(
        value.as_bool(),
        Some(true),
        "expected '{field}' = true, got: {value:?}"
    );
}

// --- Then steps: pixels ---

#[then(expr = "the image pixels response status should be {int}")]
fn pixels_status_eq(world: &mut RpWorld, expected: u16) {
    let status = world
        .last_image_pixels_status
        .expect("no image pixels response recorded");
    assert_eq!(status, expected, "unexpected pixels status");
}

#[then(expr = "the image pixels content-type should be {string}")]
fn pixels_content_type_eq(world: &mut RpWorld, expected: String) {
    let actual = world
        .last_image_pixels_content_type
        .as_deref()
        .expect("no content-type recorded");
    assert_eq!(actual, expected);
}

#[then("the image pixels header should match these constants (i32 little-endian):")]
fn pixels_header_matches_table(world: &mut RpWorld, step: &Step) {
    let body = world
        .last_image_pixels_body
        .as_ref()
        .expect("no image pixels body recorded");
    let table = step
        .table
        .as_ref()
        .expect("step requires a data table of header constants");
    // First row is the header row (field, offset, value); skip it.
    for row in table.rows.iter().skip(1) {
        assert_eq!(row.len(), 3, "table row must have 3 columns: {row:?}");
        let field = &row[0];
        let offset: usize = row[1]
            .parse()
            .unwrap_or_else(|_| panic!("offset must be a usize for '{}', got: {}", field, row[1]));
        let expected: i32 = row[2]
            .parse()
            .unwrap_or_else(|_| panic!("value must be an i32 for '{}', got: {}", field, row[2]));
        let end = offset + 4;
        assert!(
            body.len() >= end,
            "body too short ({} bytes) to read '{}' at offset {}",
            body.len(),
            field,
            offset
        );
        let bytes: [u8; 4] = body[offset..end]
            .try_into()
            .expect("4-byte slice fits in [u8; 4]");
        let actual = i32::from_le_bytes(bytes);
        assert_eq!(
            actual, expected,
            "header field '{field}' at offset {offset}"
        );
    }
}

// --- Then steps: pixel order ---

/// `ImageBytes` header offsets (all fields i32 little-endian).
const DATA_START_OFFSET: usize = 16;
const TRANSMISSION_TYPE_OFFSET: usize = 24;
const DIMENSION_1_OFFSET: usize = 32;
const DIMENSION_2_OFFSET: usize = 36;
/// Alpaca `ImageBytes` transmission element type for `UInt16`.
const TRANSMISSION_U16: i32 = 8;

#[then(
    expr = "the image pixels dimension_1 and dimension_2 should equal NAXIS1 and NAXIS2 of the FITS file at {string}"
)]
fn pixels_dimensions_match_fits(world: &mut RpWorld, field: String) {
    let (_, width, height) = fits_from_metadata(world, &field);
    let body = pixels_body(world);
    assert_eq!(
        header_i32(body, DIMENSION_1_OFFSET),
        i32::try_from(width).expect("NAXIS1 fits i32"),
        "dimension_1 = NAXIS1"
    );
    assert_eq!(
        header_i32(body, DIMENSION_2_OFFSET),
        i32::try_from(height).expect("NAXIS2 fits i32"),
        "dimension_2 = NAXIS2"
    );
}

#[then("the image pixel at (x, y) should equal the FITS pixel at row y column x for every x and y")]
fn pixels_are_the_fits_picture_width_major(world: &mut RpWorld) {
    let (fits, width, height) = fits_from_metadata(world, "fits_path");
    let body = pixels_body(world);
    assert_eq!(
        header_i32(body, TRANSMISSION_TYPE_OFFSET),
        TRANSMISSION_U16,
        "a 16-bit simulator camera is served as UInt16"
    );
    let data_start = usize::try_from(header_i32(body, DATA_START_OFFSET)).expect("data_start");
    let payload = &body[data_start..];
    assert_eq!(
        payload.len(),
        width * height * 2,
        "payload holds width × height u16 pixels"
    );
    let wire: Vec<i32> = payload
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i32::from(u16::from_le_bytes(*b)))
        .collect();
    for x in 0..width {
        for y in 0..height {
            let wire_index = x * height + y;
            let fits_index = y * width + x;
            assert_eq!(
                wire[wire_index], fits[fits_index],
                "pixel (x={x}, y={y}): wire index {wire_index} vs FITS index {fits_index}"
            );
        }
    }
    let first = fits[0];
    assert!(
        fits.iter().any(|&p| p != first),
        "the simulator frame is flat, so the pixel-order check proves nothing"
    );
}

// --- Helpers ---

fn pixels_body(world: &RpWorld) -> &[u8] {
    world
        .last_image_pixels_body
        .as_deref()
        .expect("no image pixels body recorded")
}

fn header_i32(body: &[u8], offset: usize) -> i32 {
    let bytes: [u8; 4] = body[offset..offset + 4]
        .try_into()
        .expect("4-byte header field");
    i32::from_le_bytes(bytes)
}

/// Decode the FITS file the image metadata points at (`field`, normally
/// `fits_path`) into `(row-major pixels, width, height)`.
fn fits_from_metadata(world: &RpWorld, field: &str) -> (Vec<i32>, usize, usize) {
    let path = metadata_or_panic(world)
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("image metadata has no string '{field}'"))
        .to_string();
    let file = std::fs::File::open(&path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    rp_fits::reader::read_primary_as_i32(std::io::BufReader::new(file))
        .unwrap_or_else(|e| panic!("decode {path}: {e}"))
}

const fn metadata_or_panic(world: &RpWorld) -> &Value {
    world
        .last_image_metadata
        .as_ref()
        .expect("no image metadata body recorded")
}

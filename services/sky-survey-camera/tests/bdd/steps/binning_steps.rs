//! Step definitions for `binning.feature` (contracts E3 and E8).

use crate::world::SkySurveyCameraWorld;
use cucumber::{given, then, when};
use std::time::Duration;

/// The Alpaca method behind a bin member; the regexes below admit only these.
fn bin_method(member: &str) -> &'static str {
    match member {
        "BinX" => "binx",
        "BinY" => "biny",
        other => panic!("not a bin member: {other}"),
    }
}

async fn set_bin(world: &mut SkySurveyCameraWorld, member: &str, value: u8) {
    world.last_ascom_error = None;
    world
        .put_camera(bin_method(member), &[(member, value.to_string())])
        .await;
}

/// A write the scenario needs to succeed: a refusal fails the step that made
/// it, not a later read-back.
async fn set_bin_accepted(world: &mut SkySurveyCameraWorld, member: &str, value: u8) {
    set_bin(world, member, value).await;
    if let Some(code) = world.last_ascom_error {
        panic!("setting {member} to {value} was refused with ASCOM {code:#X}");
    }
}

#[given(regex = r"^(BinX|BinY) is set to (\d+)$")]
async fn bin_is_set(world: &mut SkySurveyCameraWorld, member: String, value: u8) {
    set_bin_accepted(world, &member, value).await;
}

#[when(regex = r"^I set (BinX|BinY) to (\d+)$")]
async fn set_bin_member(world: &mut SkySurveyCameraWorld, member: String, value: u8) {
    set_bin_accepted(world, &member, value).await;
}

/// A write the scenario expects may be refused: the error is recorded for a
/// later `Then` to judge.
#[when(regex = r"^I try to set (BinX|BinY) to (\d+)$")]
async fn try_set_bin_member(world: &mut SkySurveyCameraWorld, member: String, value: u8) {
    set_bin(world, &member, value).await;
}

#[then(regex = r"^(BinX|BinY) reads (\d+)$")]
async fn bin_reads(world: &mut SkySurveyCameraWorld, member: String, expected: u8) {
    world.last_ascom_error = None;
    world.get_camera(bin_method(&member)).await;
    if let Some(code) = world.last_ascom_error {
        panic!("reading {member} was refused with ASCOM {code:#X}");
    }
    let body: serde_json::Value = serde_json::from_str(
        world
            .last_http_body
            .as_deref()
            .expect("no response body captured"),
    )
    .expect("response body not JSON");
    let actual = body["Value"]
        .as_u64()
        .unwrap_or_else(|| panic!("{member} Value missing or not an integer: {body}"));
    assert_eq!(actual, u64::from(expected), "{member} read back");
}

#[then("the write is rejected with ASCOM INVALID_VALUE")]
fn write_rejected_invalid_value(world: &mut SkySurveyCameraWorld) {
    let actual = world
        .last_ascom_error
        .expect("the write was accepted instead of refused");
    assert_eq!(
        actual, 0x401,
        "expected INVALID_VALUE (0x401), got {actual:#X} (body: {:?})",
        world.last_http_body
    );
}

/// Sets only the sub-frame extent: the bin is whatever the scenario left it
/// at, which is the point of the step.
#[when(expr = "I StartExposure with NumX {int} NumY {int}")]
async fn start_exposure_with_extent(world: &mut SkySurveyCameraWorld, num_x: u32, num_y: u32) {
    world.last_ascom_error = None;
    for (method, key, value) in [("numx", "NumX", num_x), ("numy", "NumY", num_y)] {
        world.put_camera(method, &[(key, value.to_string())]).await;
    }
    let params = [
        ("Duration", "0.1".to_string()),
        ("Light", "true".to_string()),
    ];
    world.put_camera("startexposure", &params).await;
    if let Some(code) = world.last_ascom_error {
        panic!("StartExposure rejected with ASCOM {code:#X}; expected to spawn task");
    }
}

#[then(expr = "the survey cutout was requested at {int} by {int} pixels")]
async fn cutout_requested_at(world: &mut SkySurveyCameraWorld, width: u32, height: u32) {
    // The resulting-image step before this one has already waited for the
    // fetch; this bound only covers a scenario that asks before it does.
    assert!(
        world.wait_for_image_ready(Duration::from_secs(10)).await,
        "image never became ready within 10s"
    );
    assert_eq!(
        world.stub_last_pixels().as_deref(),
        Some(format!("{width},{height}").as_str()),
        "the Pixels parameter of the last survey request"
    );
}

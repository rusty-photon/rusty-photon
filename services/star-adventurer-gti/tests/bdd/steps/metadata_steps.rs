//! Steps for `device_metadata.feature`.

use crate::world::StarAdventurerWorld;
use cucumber::gherkin::Step;
use cucumber::{given, then};

#[given(expr = "a star-adventurer service configured with name {string}")]
async fn configured_with_name(world: &mut StarAdventurerWorld, name: String) {
    world.config_mut().mount.name = name;
    world.start_service().await;
}

#[given(expr = "a star-adventurer service configured with unique ID {string}")]
async fn configured_with_unique_id(world: &mut StarAdventurerWorld, id: String) {
    world.config_mut().mount.unique_id = id;
    world.start_service().await;
}

#[given(expr = "a star-adventurer service configured with description {string}")]
async fn configured_with_description(world: &mut StarAdventurerWorld, description: String) {
    world.config_mut().mount.description = description;
    world.start_service().await;
}

#[given(expr = "a star-adventurer service configured with site latitude {float} degrees")]
async fn configured_with_site_latitude(world: &mut StarAdventurerWorld, deg: f64) {
    world.config_mut().mount.site_latitude_deg = deg;
    world.start_service().await;
}

#[given(expr = "a star-adventurer service configured with site latitude {float}")]
async fn configured_with_site_latitude_no_unit(world: &mut StarAdventurerWorld, deg: f64) {
    world.config_mut().mount.site_latitude_deg = deg;
    world.start_service().await;
}

#[given(expr = "a star-adventurer service configured with site longitude {float} degrees")]
async fn configured_with_site_longitude(world: &mut StarAdventurerWorld, deg: f64) {
    world.config_mut().mount.site_longitude_deg = deg;
    world.start_service().await;
}

#[given(expr = "a star-adventurer service configured with site longitude {float}")]
async fn configured_with_site_longitude_no_unit(world: &mut StarAdventurerWorld, deg: f64) {
    world.config_mut().mount.site_longitude_deg = deg;
    world.start_service().await;
}

#[then(expr = "the device name should be {string}")]
async fn device_name_should_be(world: &mut StarAdventurerWorld, expected: String) {
    let actual = world.mount().static_name().to_string();
    assert_eq!(actual, expected);
}

#[then(expr = "the device unique ID should be {string}")]
async fn device_unique_id_should_be(world: &mut StarAdventurerWorld, expected: String) {
    let actual = world.mount().unique_id().to_string();
    assert_eq!(actual, expected);
}

#[then(expr = "the device description should be {string}")]
async fn device_description_should_be(world: &mut StarAdventurerWorld, expected: String) {
    let actual = world.mount().description().await.unwrap();
    assert_eq!(actual, expected);
}

#[then(expr = "the driver info should contain {string}")]
async fn driver_info_should_contain(world: &mut StarAdventurerWorld, needle: String) {
    let info = world.mount().driver_info().await.unwrap();
    assert!(
        info.contains(&needle),
        "driver_info '{info}' lacks '{needle}'"
    );
}

#[then("the driver version should not be empty")]
async fn driver_version_not_empty(world: &mut StarAdventurerWorld) {
    let version = world.mount().driver_version().await.unwrap();
    assert_ne!(version, "");
}

#[then("the device capabilities should match these values:")]
async fn capabilities_should_match(world: &mut StarAdventurerWorld, step: &Step) {
    use ascom_alpaca::api::telescope::{AlignmentMode, EquatorialCoordinateType};
    let table = step.table.as_ref().expect("expected a data table");
    let mount = world.mount();
    for row in table.rows.iter().skip(1) {
        // skip header
        let cap = row[0].trim();
        let want = row[1].trim();
        match cap {
            "AlignmentMode" => {
                let v = mount.alignment_mode().await.unwrap();
                let want_enum = match want {
                    "GermanPolar" => AlignmentMode::GermanPolar,
                    "AltAz" => AlignmentMode::AltAz,
                    "Polar" => AlignmentMode::Polar,
                    other => panic!("unknown AlignmentMode {other}"),
                };
                assert_eq!(v, want_enum);
            }
            "EquatorialSystem" => {
                let v = mount.equatorial_system().await.unwrap();
                let want_enum = match want {
                    "Topocentric" => EquatorialCoordinateType::Topocentric,
                    "J2000" => EquatorialCoordinateType::J2000,
                    other => panic!("unknown EquatorialSystem {other}"),
                };
                assert_eq!(v, want_enum);
            }
            "CanSlew" => assert_eq!(mount.can_slew().await.unwrap(), parse_bool(want)),
            "CanSlewAsync" => assert_eq!(mount.can_slew_async().await.unwrap(), parse_bool(want)),
            "CanSlewAltAz" => assert_eq!(mount.can_slew_alt_az().await.unwrap(), parse_bool(want)),
            "CanSlewAltAzAsync" => {
                assert_eq!(
                    mount.can_slew_alt_az_async().await.unwrap(),
                    parse_bool(want)
                );
            }
            "CanSync" => assert_eq!(mount.can_sync().await.unwrap(), parse_bool(want)),
            "CanSyncAltAz" => assert_eq!(mount.can_sync_alt_az().await.unwrap(), parse_bool(want)),
            "CanSetTracking" => {
                assert_eq!(mount.can_set_tracking().await.unwrap(), parse_bool(want));
            }
            "CanSetRightAscensionRate" => assert_eq!(
                mount.can_set_right_ascension_rate().await.unwrap(),
                parse_bool(want)
            ),
            "CanSetDeclinationRate" => assert_eq!(
                mount.can_set_declination_rate().await.unwrap(),
                parse_bool(want)
            ),
            "CanSetGuideRates" => {
                assert_eq!(mount.can_set_guide_rates().await.unwrap(), parse_bool(want));
            }
            "CanPulseGuide" => {
                assert_eq!(mount.can_pulse_guide().await.unwrap(), parse_bool(want));
            }
            "CanFindHome" => assert_eq!(mount.can_find_home().await.unwrap(), parse_bool(want)),
            "CanPark" => assert_eq!(mount.can_park().await.unwrap(), parse_bool(want)),
            "CanUnpark" => assert_eq!(mount.can_unpark().await.unwrap(), parse_bool(want)),
            "CanSetPark" => assert_eq!(mount.can_set_park().await.unwrap(), parse_bool(want)),
            "CanSetPierSide" => {
                assert_eq!(mount.can_set_pier_side().await.unwrap(), parse_bool(want));
            }
            "DoesRefraction" => {
                assert_eq!(mount.does_refraction().await.unwrap(), parse_bool(want));
            }
            other => panic!("capabilities table has unknown column {other:?}"),
        }
    }
}

fn parse_bool(s: &str) -> bool {
    match s {
        "true" => true,
        "false" => false,
        other => panic!("expected true/false, got {other}"),
    }
}

#[then("TrackingRates should equal [Sidereal]")]
async fn tracking_rates_sidereal_only(world: &mut StarAdventurerWorld) {
    use ascom_alpaca::api::telescope::DriveRate;
    let rates = world.mount().tracking_rates().await.unwrap();
    assert_eq!(rates, vec![DriveRate::Sidereal]);
}

#[then(expr = "SiteLatitude should be {float} degrees")]
async fn site_latitude_should_be(world: &mut StarAdventurerWorld, expected: f64) {
    let actual = world.mount().site_latitude().await.unwrap();
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

#[then(expr = "SiteLongitude should be {float} degrees")]
async fn site_longitude_should_be(world: &mut StarAdventurerWorld, expected: f64) {
    let actual = world.mount().site_longitude().await.unwrap();
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

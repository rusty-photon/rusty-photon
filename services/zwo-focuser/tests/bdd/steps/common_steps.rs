//! Shared steps: service startup, connection lifecycle, boolean property
//! reports, and the generic rejection assertion.

use cucumber::{given, then, when};

use crate::world::{ascom_code, FocuserWorld};

// --- service startup --------------------------------------------------------

#[given("the zwo-focuser service running with the simulation backend")]
#[given("a running zwo-focuser service with the simulation backend")]
async fn service_running(world: &mut FocuserWorld) {
    world.start().await;
}

#[given("the zwo-focuser service running with an empty simulation backend")]
async fn service_running_empty(world: &mut FocuserWorld) {
    world.empty_backend = true;
    world.start().await;
}

// --- connection lifecycle ---------------------------------------------------

#[given(regex = r"^focuser device (\d+) is connected$")]
async fn focuser_is_connected(world: &mut FocuserWorld, _device: u32) {
    world.focuser().set_connected(true).await.unwrap();
}

#[given(regex = r"^focuser device (\d+) is not connected$")]
async fn focuser_is_not_connected(world: &mut FocuserWorld, _device: u32) {
    world.focuser().set_connected(false).await.unwrap();
}

#[when(regex = r"^I connect focuser device (\d+)$")]
async fn connect_focuser(world: &mut FocuserWorld, _device: u32) {
    world.focuser().set_connected(true).await.unwrap();
}

#[when(regex = r"^I disconnect focuser device (\d+)$")]
async fn disconnect_focuser(world: &mut FocuserWorld, _device: u32) {
    world.focuser().set_connected(false).await.unwrap();
}

// --- boolean property reports (used as Given precondition and Then check) ----

#[given(regex = r"^focuser device (\d+) reports (\w+) as (true|false)$")]
#[then(regex = r"^focuser device (\d+) reports (\w+) as (true|false)$")]
async fn focuser_reports_bool(
    world: &mut FocuserWorld,
    _device: u32,
    property: String,
    expected: bool,
) {
    let focuser = world.focuser();
    let actual = match property.as_str() {
        "Connected" => focuser.connected().await.unwrap(),
        "IsMoving" => focuser.is_moving().await.unwrap(),
        "Absolute" => focuser.absolute().await.unwrap(),
        "TempComp" => focuser.temp_comp().await.unwrap(),
        "TempCompAvailable" => focuser.temp_comp_available().await.unwrap(),
        other => panic!("unknown boolean property: {other}"),
    };
    assert_eq!(
        actual, expected,
        "{property} expected {expected}, got {actual}"
    );
}

// --- enumeration / health ---------------------------------------------------

#[then(regex = r"^ASCOM focuser device (\d+) is available$")]
async fn focuser_is_available(world: &mut FocuserWorld, _device: u32) {
    assert!(
        world.focuser.is_some(),
        "focuser device {_device} not registered"
    );
}

#[then(regex = r"^focuser device (\d+) reports a non-empty UniqueID$")]
async fn focuser_non_empty_unique_id(world: &mut FocuserWorld, _device: u32) {
    // `unique_id` is a sync `Device` member (not an HTTP round-trip).
    let focuser = world.focuser();
    assert_ne!(focuser.unique_id(), "");
}

#[then("no ASCOM focuser devices are registered")]
async fn no_focusers_registered(world: &mut FocuserWorld) {
    assert!(world.focuser.is_none(), "expected no Focuser devices");
}

#[then("the service is healthy")]
async fn service_healthy(world: &mut FocuserWorld) {
    assert!(world.management_responds().await, "service did not respond");
}

// --- generic rejection assertion ---------------------------------------------

#[then(regex = r"^the move is rejected with ASCOM (\w+)$")]
async fn rejected_with(world: &mut FocuserWorld, code: String) {
    assert_eq!(
        world.last_error_code,
        Some(ascom_code(&code)),
        "expected {code}, got {:?}",
        world.last_error_code
    );
}

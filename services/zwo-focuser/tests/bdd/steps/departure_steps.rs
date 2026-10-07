//! An EAF that leaves the bus (C5): starting the service over a simulated
//! focuser that can, taking the focuser away and giving it back, and the
//! reconnect a client makes once it has gone.

use cucumber::gherkin::Step;
use cucumber::{given, then, when};

use crate::world::{ascom_code, FocuserWorld};

#[given("the zwo-focuser service running with a simulated focuser that can leave the bus")]
async fn service_with_a_departing_focuser(world: &mut FocuserWorld) {
    world.departure_file = Some(world.scratch_dir().join("departed"));
    world.start().await;
}

#[when(regex = r"^focuser device (\d+) leaves the bus$")]
async fn focuser_leaves_the_bus(world: &mut FocuserWorld, _device: u32) {
    let departure = world
        .departure_file
        .as_ref()
        .expect("the service was not started with a departure file");
    std::fs::write(departure, b"").expect("write the departure file");
}

#[when(regex = r"^focuser device (\d+) returns to the bus$")]
async fn focuser_returns_to_the_bus(world: &mut FocuserWorld, _device: u32) {
    let departure = world
        .departure_file
        .as_ref()
        .expect("the service was not started with a departure file");
    std::fs::remove_file(departure).expect("remove the departure file");
}

#[when(regex = r"^I try to connect focuser device (\d+)$")]
async fn try_connect_focuser(world: &mut FocuserWorld, _device: u32) {
    world.last_error_code = world
        .focuser()
        .set_connected(true)
        .await
        .err()
        .map(|e| e.code.raw());
}

#[then(regex = r"^the call is rejected with ASCOM (\w+)$")]
async fn call_rejected_with(world: &mut FocuserWorld, code: String) {
    assert_eq!(
        world.last_error_code,
        Some(ascom_code(&code)),
        "expected {code}, got {:?}",
        world.last_error_code
    );
}

#[then(regex = r"^reading these members from focuser device (\d+) is rejected with ASCOM (\w+):$")]
async fn members_rejected_with(world: &mut FocuserWorld, step: &Step, _device: u32, code: String) {
    let table = step.table.as_ref().expect("a table of members");
    let focuser = world.focuser();
    let expected = ascom_code(&code);
    for row in table.rows.iter().skip(1) {
        let member = row[0].as_str();
        let answer = match member {
            "Position" => focuser.position().await.map(drop),
            "IsMoving" => focuser.is_moving().await.map(drop),
            "Temperature" => focuser.temperature().await.map(drop),
            "MaxStep" => focuser.max_step().await.map(drop),
            "MaxIncrement" => focuser.max_increment().await.map(drop),
            other => panic!("unknown member: {other}"),
        };
        let got = answer.err().map(|e| e.code.raw());
        assert_eq!(
            got,
            Some(expected),
            "{member}: expected {code}, got {got:?}"
        );
    }
}

#[then(regex = r"^moving focuser device (\d+) to position (-?\d+) is rejected with ASCOM (\w+)$")]
async fn move_rejected_with(world: &mut FocuserWorld, _device: u32, position: i32, code: String) {
    world.try_move(position).await;
    call_rejected_with(world, code).await;
}

#[then(regex = r"^halting focuser device (\d+) is rejected with ASCOM (\w+)$")]
async fn halt_rejected_with(world: &mut FocuserWorld, _device: u32, code: String) {
    world.try_halt().await;
    call_rejected_with(world, code).await;
}

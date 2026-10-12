// Manual hardware-probe binary (requires a physical camera) — excluded from
// coverage.
//
// What the SDK reports about each camera with nothing opened: its id
// (`GetQHYCCDId`) and its model (`GetQHYCCDModel`, read from the id). Prints one
// JSON line per camera, and fails if any camera reads open afterwards, since
// nothing here may open one.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
// Dying loudly on a missing device or an unmet precondition is the intended
// failure mode of a hand-run probe (the zwo-rs probe-example convention); the
// JSON lines are its output.
#![expect(clippy::expect_used, clippy::print_stdout)]
use qhyccd_rs::Sdk;

fn main() {
    let sdk = Sdk::new().expect("SDK::new failed");
    assert!(
        sdk.cameras().next().is_some(),
        "precondition: a camera on the bus"
    );
    for (index, camera) in sdk.cameras().enumerate() {
        // `{:?}` quotes and escapes a string, which is a JSON string for the
        // ASCII ids and models the SDK reports.
        let model = match camera.get_model() {
            Ok(model) => format!("\"model\":{model:?}"),
            Err(error) => format!("\"model_error\":{:?}", error.to_string()),
        };
        println!(
            "{{\"index\":{index},\"id\":{:?},{model},\"open\":{}}}",
            camera.id(),
            camera.is_open().expect("open state")
        );
    }
    assert!(
        sdk.cameras().all(|c| !c.is_open().expect("open state")),
        "enumeration and the model read open no camera"
    );
}

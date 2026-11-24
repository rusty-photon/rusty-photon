//! Steps of `health.feature`.

use cucumber::{then, when};
use serde_json::{json, Value};

use crate::steps::config_steps::json_value;
use crate::world::GuiderWorld;

#[when(regex = r"^/health answers 200 with the body (.+)$")]
#[then(regex = r"^/health answers 200 with the body (.+)$")]
async fn healthy(world: &mut GuiderWorld, body: String) {
    let expected = json_value(&body);
    world
        .wait_for("/health", &format!("200 {expected}"), |status, answer| {
            status == 200 && *answer == expected
        })
        .await;
}

#[then(expr = "\\/health answers 503 with the message {string}")]
async fn unhealthy(world: &mut GuiderWorld, message: String) {
    let expected: Value = json!({ "status": "unavailable", "message": message });
    world
        .wait_for("/health", &format!("503 {expected}"), |status, answer| {
            status == 503 && *answer == expected
        })
        .await;
}

//! Steps of `calibration_store.feature`: `GET /api/v1/calibration`,
//! `calibration/clear` and the file in the state directory.

use std::path::PathBuf;

use cucumber::{given, then, when};
use serde_json::{json, Value};

use crate::steps::loop_steps::latest;
use crate::world::GuiderWorld;

/// The configuration's state directory.
fn state_dir(world: &GuiderWorld) -> PathBuf {
    PathBuf::from(
        world.config["state_dir"]
            .as_str()
            .expect("the configuration names a state directory"),
    )
}

/// The store as `GET /api/v1/calibration` answers it.
async fn store(world: &GuiderWorld) -> Value {
    let (status, body) = world
        .get("/api/v1/calibration")
        .await
        .expect("GET /api/v1/calibration");
    assert_eq!(
        status, 200,
        "GET /api/v1/calibration answered {status}: {body}"
    );
    body
}

#[when("a client asks for the calibration store")]
async fn ask_for_store(world: &mut GuiderWorld) {
    let answer = world
        .request(reqwest::Method::GET, "/api/v1/calibration", None)
        .await
        .unwrap_or_else(|e| panic!("GET {e}"));
    world.answer = Some(answer);
}

#[when("the guider is started again")]
async fn started_again(world: &mut GuiderWorld) {
    world.restart_guider().await;
    assert!(
        world.guider.is_some(),
        "the guider did not start again: {:?}",
        world.refusal
    );
}

#[given(regex = r#"^the state directory holds a calibration.json of "(.*)"$"#)]
fn calibration_file_of(world: &mut GuiderWorld, text: String) {
    let dir = state_dir(world);
    std::fs::create_dir_all(&dir).expect("create the state directory");
    std::fs::write(dir.join("calibration.json"), text).expect("write calibration.json");
}

#[then(
    regex = r#"^the guider's stored calibration for the "(east|west|unknown)" pier side is the one answered$"#
)]
async fn stored_is_answered(world: &mut GuiderWorld, side: String) {
    let answered = latest(world).json()["calibration"].clone();
    assert!(
        answered.is_object(),
        "no calibration was answered: {answered}"
    );
    let stored = store(world).await;
    assert_eq!(
        stored[side.as_str()],
        answered,
        "the {side} slot of {stored}"
    );
}

#[then("the guider stores no calibration")]
async fn stores_nothing(world: &mut GuiderWorld) {
    assert_eq!(
        store(world).await,
        json!({ "east": null, "west": null, "unknown": null })
    );
}

#[then("the state directory's calibration.json holds the store answered")]
fn file_holds_store(world: &mut GuiderWorld) {
    let answered = latest(world).json().clone();
    let text = std::fs::read_to_string(state_dir(world).join("calibration.json"))
        .expect("read calibration.json");
    let file: Value = serde_json::from_str(&text).expect("calibration.json is JSON");
    assert_eq!(
        file, answered,
        "calibration.json against the store answered"
    );
    assert!(
        answered["east"].is_object(),
        "the store holds no east record: {answered}"
    );
}

#[then("the state directory's calibration.json holds no record")]
fn file_holds_nothing(world: &mut GuiderWorld) {
    let text = std::fs::read_to_string(state_dir(world).join("calibration.json"))
        .expect("read calibration.json");
    let file: Value = serde_json::from_str(&text).expect("calibration.json is JSON");
    assert_eq!(file, json!({ "east": null, "west": null, "unknown": null }));
}

#[then(
    regex = r#"^the state directory holds a file set aside from calibration.json holding "(.*)"$"#
)]
fn set_aside(world: &mut GuiderWorld, text: String) {
    let dir = state_dir(world);
    let aside: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read the state directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("calibration.json.unreadable-"))
        })
        .collect();
    assert_eq!(
        aside.len(),
        1,
        "the files set aside in {}: {aside:?}",
        dir.display()
    );
    let held = std::fs::read_to_string(&aside[0]).expect("read the file set aside");
    assert_eq!(held, text, "the file set aside");
}

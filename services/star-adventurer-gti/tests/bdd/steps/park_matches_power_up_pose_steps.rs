//! Steps for `park_matches_power_up_pose.feature`: the mount's Actions,
//! `SupportedActions`, and the checks that the Action moves nothing and
//! writes nothing.

use std::sync::Arc;

use ascom_alpaca::ASCOMErrorCode;
use cucumber::gherkin::Step;
use cucumber::{then, when};
use serde_json::{Map, Value};

use crate::world::{NotedFile, StarAdventurerWorld};

/// The Action's name, as `SupportedActions` spells it.
const PARK_MATCHES_POWER_UP_POSE: &str = "ParkMatchesPowerUpPose";

/// The frames the background poll sends: a position and a status read
/// per axis. Nothing in them moves the mount or writes to it.
const POLL_READS: [&str; 4] = [":j1\r", ":j2\r", ":f1\r", ":f2\r"];

/// Call `name` with no parameters, keeping the answer or the error code.
async fn call_action(world: &mut StarAdventurerWorld, name: String) {
    let mount = Arc::clone(world.mount());
    match mount.action(name, String::new()).await {
        Ok(answer) => {
            world.clear_error();
            world.last_action_answer = Some(answer);
        }
        Err(e) => {
            world.last_action_answer = None;
            world.record_error(e);
        }
    }
}

/// The last answer, parsed: the Action answers a JSON object.
fn answer(world: &StarAdventurerWorld) -> Value {
    let text = world.last_action_answer.as_ref().unwrap_or_else(|| {
        panic!(
            "no Action answered; last error {:?} ({:?})",
            world.last_error_code, world.last_error
        )
    });
    serde_json::from_str(text).unwrap_or_else(|e| panic!("the answer is not JSON ({e}): {text}"))
}

fn parse_json(cell: &str) -> Value {
    serde_json::from_str(cell.trim())
        .unwrap_or_else(|e| panic!("feature-file value {cell:?} is not JSON: {e}"))
}

#[when("the supported actions are queried")]
async fn query_supported_actions(world: &mut StarAdventurerWorld) {
    let mount = Arc::clone(world.mount());
    world.last_supported_actions = Some(mount.supported_actions().await.unwrap());
}

#[then(expr = "the supported actions are exactly {string}")]
fn assert_supported_actions(world: &mut StarAdventurerWorld, name: String) {
    let actions = world
        .last_supported_actions
        .as_ref()
        .expect("no supported actions queried");
    assert_eq!(actions, &vec![name], "SupportedActions");
}

#[when(regex = r#"^the action "([^"]+)" is called$"#)]
async fn call_named_action(world: &mut StarAdventurerWorld, name: String) {
    call_action(world, name).await;
}

#[when("I ask whether the park matches the power-up pose")]
async fn ask_park_matches(world: &mut StarAdventurerWorld) {
    call_action(world, PARK_MATCHES_POWER_UP_POSE.to_string()).await;
    assert!(
        world.last_action_answer.is_some(),
        "{PARK_MATCHES_POWER_UP_POSE} failed: {:?} ({:?})",
        world.last_error_code,
        world.last_error
    );
}

#[when("I try to ask whether the park matches the power-up pose")]
async fn try_ask_park_matches(world: &mut StarAdventurerWorld) {
    call_action(world, PARK_MATCHES_POWER_UP_POSE.to_string()).await;
}

#[then("the call should fail with an action-not-implemented error")]
fn assert_action_not_implemented(world: &mut StarAdventurerWorld) {
    assert_eq!(
        world.last_error_code,
        Some(ASCOMErrorCode::ACTION_NOT_IMPLEMENTED.raw()),
        "expected ACTION_NOT_IMPLEMENTED, got {:?} (answer {:?})",
        world.last_error_code,
        world.last_action_answer
    );
}

/// The answer is the object the table spells, field for field: a field
/// the table does not name fails the step as surely as a wrong value.
#[then("the action answers the JSON object:")]
fn assert_answer_object(world: &mut StarAdventurerWorld, step: &Step) {
    let table = step.table.as_ref().expect("expected a data table");
    let expected: Map<String, Value> = table
        .rows
        .iter()
        .skip(1)
        .map(|row| (row[0].trim().to_string(), parse_json(&row[1])))
        .collect();
    assert_eq!(
        answer(world),
        Value::Object(expected),
        "the Action's answer"
    );
}

#[then(regex = r#"^the answer's "([^"]+)" field is (.+)$"#)]
fn assert_answer_field(world: &mut StarAdventurerWorld, field: String, value: String) {
    let answer = answer(world);
    let got = answer
        .get(&field)
        .unwrap_or_else(|| panic!("the answer has no field {field}: {answer}"));
    assert_eq!(got, &parse_json(&value), "field {field} of {answer}");
}

#[when(expr = "the mount reports both axes stopped at RA {int} and Dec {int} ticks")]
async fn axes_stopped_at(world: &mut StarAdventurerWorld, ra: i32, dec: i32) {
    world.queue_seed("ra_ticks", ra.into()).await;
    world.queue_seed("dec_ticks", dec.into()).await;
    world.queue_seed("ra_running", false.into()).await;
    world.queue_seed("dec_running", false.into()).await;
}

#[when("I note the configuration file, the mount state record and the command log")]
async fn note_files_and_log(world: &mut StarAdventurerWorld) {
    world.noted_files = vec![
        NotedFile::read(world.config_file_path()),
        NotedFile::read(world.mount_state_record_path()),
    ];
    world.noted_log_len = Some(world.command_log_including_startup().await.len());
}

/// A write replaces a file's bytes or its modification time: the driver
/// writes both files through a temp file renamed over the old one, so
/// even a write of the same bytes leaves a newer time.
#[then("the configuration file and the mount state record should be as noted")]
fn assert_files_as_noted(world: &mut StarAdventurerWorld) {
    assert!(!world.noted_files.is_empty(), "no files noted");
    for noted in &world.noted_files {
        let now = NotedFile::read(noted.path.clone());
        assert_eq!(
            now.bytes,
            noted.bytes,
            "{} changed:\nnow {}\nnoted {}",
            noted.path.display(),
            String::from_utf8_lossy(&now.bytes),
            String::from_utf8_lossy(&noted.bytes)
        );
        assert_eq!(
            now.modified,
            noted.modified,
            "{} was written again",
            noted.path.display()
        );
    }
}

#[then("the mount should have received only position and status reads since the note")]
async fn only_reads_since_note(world: &mut StarAdventurerWorld) {
    let mark = world.noted_log_len.expect("the command log was noted");
    let log = world.command_log_including_startup().await;
    let others: Vec<&String> = log
        .get(mark..)
        .unwrap_or(&[])
        .iter()
        .filter(|c| !POLL_READS.contains(&c.as_str()))
        .collect();
    assert!(
        others.is_empty(),
        "expected only {POLL_READS:?} since the note, saw {others:?}"
    );
}

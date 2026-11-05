//! Step definitions for `power_cut.feature`.

use std::time::Duration;

use cucumber::{then, when};
use serde_json::{json, Value};
use tokio::time::sleep;

use crate::world::{CommandLogTimeout, StarAdventurerWorld};

/// How long the reconnect may take once the controller answers again:
/// the shared transport retries every 5 s, and a loaded runner adds to
/// that.
const RECONNECT_WINDOW: Duration = Duration::from_secs(30);

/// Long enough for the driver to poll the mount several times (the BDD
/// config polls every 50 ms), so its last sample reflects the state the
/// scenario set up before the cut, and so the cut fails a poll.
const POLLS: Duration = Duration::from_millis(400);

/// Wait until the frames logged after `mark` hold a completed reconnect:
/// the handshake's `:F2`, then the first background poll's `:f2` after
/// it. The poll starts only once the transport serves again, so by then
/// any position the handshake wrote back is on the wire.
async fn wait_for_reconnect(world: &StarAdventurerWorld, mark: usize) {
    let done = |log: &[String]| {
        let after = log.get(mark..).unwrap_or(&[]);
        after
            .iter()
            .position(|c| c == ":F2\r")
            .is_some_and(|f| after[f..].iter().any(|c| c == ":f2\r"))
    };
    match world
        .wait_for_command_log_including_startup(RECONNECT_WINDOW, done)
        .await
    {
        Ok(_) => {}
        Err(CommandLogTimeout::NoMatch(log)) => panic!(
            "the driver did not re-initialise the mount within {RECONNECT_WINDOW:?}; frames after the mark: {:?}",
            log.get(mark..).unwrap_or(&[])
        ),
        Err(CommandLogTimeout::FetchFailed(e)) => {
            panic!("could not read the command log while waiting for the reconnect: {e}")
        }
    }
}

fn ticks(v: &Value, key: &str) -> i64 {
    v[key]
        .as_i64()
        .unwrap_or_else(|| panic!("mock state lacks integer {key}: {v}"))
}

#[when("the mount loses power and comes back")]
async fn mount_loses_power(world: &mut StarAdventurerWorld) {
    sleep(POLLS).await;
    let off = world.post_debug("mock-power", json!({ "on": false })).await;
    world.ticks_at_cut = Some((ticks(&off, "ra_ticks"), ticks(&off, "dec_ticks")));
    sleep(POLLS).await;
    let on = world.post_debug("mock-power", json!({ "on": true })).await;
    let mark = usize::try_from(ticks(&on, "log_len")).expect("log length fits usize");
    world.reconnect_mark = Some(mark);
    wait_for_reconnect(world, mark).await;
}

#[when("the link to the mount drops while the mount keeps its power")]
async fn link_drops(world: &mut StarAdventurerWorld) {
    sleep(POLLS).await;
    let dropped = world.post_debug("mock-link-drop", json!({})).await;
    let mark = usize::try_from(ticks(&dropped, "log_len")).expect("log length fits usize");
    world.reconnect_mark = Some(mark);
    wait_for_reconnect(world, mark).await;
}

#[then(expr = "the mount's encoders should read RA {int} and Dec {int} ticks")]
async fn encoders_read(world: &mut StarAdventurerWorld, ra: i64, dec: i64) {
    let state = world.get_debug("mock-state").await;
    assert_eq!(
        (ticks(&state, "ra_ticks"), ticks(&state, "dec_ticks")),
        (ra, dec),
        "mock encoders"
    );
}

#[then(
    expr = "the mount's encoders should be within {int} ticks of where they stood when the power went"
)]
async fn encoders_near_cut(world: &mut StarAdventurerWorld, tolerance: i64) {
    let (ra_cut, dec_cut) = world.ticks_at_cut.expect("a power cut ran");
    let state = world.get_debug("mock-state").await;
    let (ra, dec) = (ticks(&state, "ra_ticks"), ticks(&state, "dec_ticks"));
    assert!(
        (ra - ra_cut).abs() <= tolerance && (dec - dec_cut).abs() <= tolerance,
        "encoders ({ra}, {dec}) are not within {tolerance} ticks of ({ra_cut}, {dec_cut}) at the cut"
    );
}

#[then("the mount should not have received an encoder write since the link came back")]
async fn no_encoder_write_since_mark(world: &mut StarAdventurerWorld) {
    let mark = world.reconnect_mark.expect("the link dropped");
    let log = world.command_log_including_startup().await;
    let writes: Vec<&String> = log
        .get(mark..)
        .unwrap_or(&[])
        .iter()
        .filter(|c| c.starts_with(":E"))
        .collect();
    assert!(
        writes.is_empty(),
        "expected no :E encoder writes after the link came back, saw {writes:?}"
    );
}

/// Poll the mount state record until `accept` takes it: the driver
/// writes it from a task, a poll interval or so after the change.
async fn wait_for_record(world: &StarAdventurerWorld, accept: impl Fn(&Value) -> bool) -> Value {
    let path = world.mount_state_record_path();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut last = None;
    loop {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let record: Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("record {} is not JSON ({e}): {text}", path.display()));
            if accept(&record) {
                return record;
            }
            last = Some(record);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mount state record {} never matched; last read {last:?}",
            path.display()
        );
        sleep(Duration::from_millis(50)).await;
    }
}

#[then(expr = "the mount state record should say at rest at RA {int} and Dec {int} ticks")]
async fn record_at_rest_at(world: &mut StarAdventurerWorld, ra: i64, dec: i64) {
    wait_for_record(world, |r| {
        r["state"] == "at_rest" && r["ra_ticks"] == ra && r["dec_ticks"] == dec
    })
    .await;
}

#[then(expr = "the mount state record should say at rest away from RA {int} and Dec {int} ticks")]
async fn record_at_rest_away_from(world: &mut StarAdventurerWorld, ra: i64, dec: i64) {
    wait_for_record(world, |r| {
        r["state"] == "at_rest" && (r["ra_ticks"] != ra || r["dec_ticks"] != dec)
    })
    .await;
}

#[then(expr = "the mount state record should say {string}")]
async fn record_says(world: &mut StarAdventurerWorld, state: String) {
    wait_for_record(world, |r| r["state"] == state.as_str()).await;
}

#[when("the service restarts with the mount freshly powered up")]
async fn service_restarts(world: &mut StarAdventurerWorld) {
    // Each process gets a fresh mock mount: encoders at (0, 0), axes
    // uninitialised — a power-up, as far as the new driver can tell.
    let mut handle = world.service_handle.take().expect("a service is running");
    handle.stop().await;
    world.start_service().await;
}

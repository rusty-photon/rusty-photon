//! Steps of `stats_and_metrics.feature`: `guiding/stats` and
//! `guiding/metrics`.

use cucumber::gherkin::Step;
use cucumber::{given, then};
use serde_json::Value;

use crate::steps::config_steps::json_value;
use crate::world::GuiderWorld;

/// The size of the metrics window.
const WINDOW: usize = 50;

/// `GET /api/v1/guiding/metrics`, answered 200.
pub async fn metrics(world: &GuiderWorld) -> (u16, Value) {
    let (status, body) = world
        .get("/api/v1/guiding/metrics")
        .await
        .expect("GET /api/v1/guiding/metrics");
    assert_eq!(
        status, 200,
        "GET /api/v1/guiding/metrics answered {status}: {body}"
    );
    (status, body)
}

/// The number of the newest frame in the metrics window.
pub async fn last_frame(world: &GuiderWorld) -> Option<u64> {
    let (_, body) = metrics(world).await;
    frame_numbers(&body).last().copied()
}

/// The frame numbers of a metrics answer, oldest first.
fn frame_numbers(body: &Value) -> Vec<u64> {
    body["frames"]
        .as_array()
        .map(|frames| {
            frames
                .iter()
                .map(|frame| frame["frame"].as_u64().expect("a frame number"))
                .collect()
        })
        .unwrap_or_default()
}

/// The frames of a metrics answer.
fn frames(body: &Value) -> &[Value] {
    body["frames"].as_array().map_or(&[], Vec::as_slice)
}

/// Whether `value` is a number above zero.
fn positive(value: &Value) -> bool {
    value.as_f64().is_some_and(|v| v > 0.0)
}

#[then("the guider's stats report:")]
async fn stats_report(world: &mut GuiderWorld, step: &Step) {
    let (status, body) = world
        .get("/api/v1/guiding/stats")
        .await
        .expect("GET /api/v1/guiding/stats");
    assert_eq!(
        status, 200,
        "GET /api/v1/guiding/stats answered {status}: {body}"
    );
    let table = step.table.as_ref().expect("a table of fields and values");
    for row in table.rows.iter().skip(1) {
        let (field, expected) = (&row[0], json_value(&row[1]));
        assert_eq!(
            body[field.as_str()],
            expected,
            "the stats' {field} in {body}"
        );
    }
}

#[then("the guider's stats report running totals of zero under an epoch")]
async fn totals_of_zero(world: &mut GuiderWorld) {
    let (_, body) = world
        .get("/api/v1/guiding/stats")
        .await
        .expect("GET /api/v1/guiding/stats");
    let totals = &body["totals"];
    let epoch = totals["epoch"].as_str().unwrap_or_default();
    uuid::Uuid::parse_str(epoch)
        .unwrap_or_else(|e| panic!("the epoch {epoch:?} is not a UUID: {e}"));
    for field in [
        "guide_steps",
        "ra_samples",
        "dec_samples",
        "star_lost",
        "interruptions",
    ] {
        assert_eq!(
            totals[field],
            Value::from(0),
            "the totals' {field} in {body}"
        );
    }
    for field in ["ra_sum_sq_px2", "dec_sum_sq_px2"] {
        assert_eq!(
            totals[field],
            Value::from(0.0),
            "the totals' {field} in {body}"
        );
    }
}

#[then("the guider's stats report a positive snr and star_mass")]
async fn positive_snr_and_mass(world: &mut GuiderWorld) {
    let (_, body) = world
        .get("/api/v1/guiding/stats")
        .await
        .expect("GET /api/v1/guiding/stats");
    assert!(
        positive(&body["snr"]) && positive(&body["star_mass"]),
        "the stats' snr and star_mass in {body}"
    );
}

#[given(regex = r"^the guider's metrics hold at least (\d+) frames$")]
#[then(regex = r"^the guider's metrics hold at least (\d+) frames$")]
async fn hold_at_least(world: &mut GuiderWorld, count: usize) {
    let (_, body) = world
        .wait_for(
            "/api/v1/guiding/metrics",
            &format!("{count} frames"),
            |status, body| status == 200 && frames(body).len() >= count,
        )
        .await;
    world.noted_frames = frame_numbers(&body);
}

#[then(
    regex = r"^the guider's metrics hold at least (\d+) frames, each star-lost with no usable star$"
)]
async fn hold_star_lost(world: &mut GuiderWorld, count: usize) {
    let (_, body) = world
        .wait_for(
            "/api/v1/guiding/metrics",
            &format!("{count} frames"),
            |status, body| status == 200 && frames(body).len() >= count,
        )
        .await;
    for frame in frames(&body) {
        assert_eq!(
            (&frame["star_lost"], &frame["star_count"], &frame["hfd"]),
            (&Value::Bool(true), &Value::from(0), &Value::Null),
            "a frame in {body}"
        );
    }
}

#[then(
    regex = r"^the guider's metrics hold at least (\d+) frames, each with (\d+) usable stars and a positive hfd, snr and star_mass$"
)]
async fn hold_usable(world: &mut GuiderWorld, count: usize, stars: u64) {
    let (_, body) = world
        .wait_for(
            "/api/v1/guiding/metrics",
            &format!("{count} frames"),
            |status, body| status == 200 && frames(body).len() >= count,
        )
        .await;
    for frame in frames(&body) {
        assert_eq!(
            (&frame["star_lost"], &frame["star_count"]),
            (&Value::Bool(false), &Value::from(stars)),
            "a frame in {body}"
        );
        assert!(
            positive(&frame["hfd"]) && positive(&frame["snr"]) && positive(&frame["star_mass"]),
            "a frame's hfd, snr and star_mass in {body}"
        );
    }
}

#[then("the guider's metrics number their frames one after another, oldest first")]
async fn numbered_in_order(world: &mut GuiderWorld) {
    let (_, body) = metrics(world).await;
    let numbers = frame_numbers(&body);
    assert!(!numbers.is_empty(), "the metrics hold no frame: {body}");
    for pair in numbers.windows(2) {
        assert_eq!(pair[1], pair[0] + 1, "the frame numbers in {body}");
    }
}

#[then(
    regex = r#"^the guider's metrics report the state "([a-z_]+)" under the epoch of the running totals$"#
)]
async fn metrics_state_and_epoch(world: &mut GuiderWorld, state: String) {
    let (_, metrics) = metrics(world).await;
    let (_, stats) = world
        .get("/api/v1/guiding/stats")
        .await
        .expect("GET /api/v1/guiding/stats");
    assert_eq!(metrics["state"], Value::from(state), "{metrics}");
    assert_eq!(metrics["guiding"], Value::Bool(false), "{metrics}");
    assert_eq!(
        metrics["epoch"], stats["totals"]["epoch"],
        "{metrics} {stats}"
    );
}

#[then("the guider's metrics come to hold 50 frames and no more")]
async fn window_of_fifty(world: &mut GuiderWorld) {
    let beyond = (WINDOW + 10) as u64;
    let (_, body) = world
        .wait_for(
            "/api/v1/guiding/metrics",
            &format!("frame {beyond}"),
            |status, body| {
                status == 200
                    && frame_numbers(body)
                        .last()
                        .is_some_and(|last| *last >= beyond)
            },
        )
        .await;
    let numbers = frame_numbers(&body);
    assert_eq!(numbers.len(), WINDOW, "the window in {body}");
    assert_eq!(
        numbers[0] + (WINDOW as u64) - 1,
        numbers[WINDOW - 1],
        "the window holds the newest frames: {body}"
    );
}

#[then("the guider's metrics still hold the frames they held before the request")]
async fn still_hold(world: &mut GuiderWorld) {
    let (_, body) = metrics(world).await;
    let numbers = frame_numbers(&body);
    let oldest = world.noted_frames.first().expect("the metrics were noted");
    assert!(
        numbers.contains(oldest),
        "the window lost frame {oldest}, held before the request: {body}"
    );
}

#[then("the guider's metrics hold only frames numbered above those they held before the request")]
async fn only_newer(world: &mut GuiderWorld) {
    let newest = *world.noted_frames.last().expect("the metrics were noted");
    let (_, body) = metrics(world).await;
    let numbers = frame_numbers(&body);
    assert!(!numbers.is_empty(), "the metrics hold no frame: {body}");
    assert!(
        numbers.iter().all(|number| *number > newest),
        "the window kept frames from before the request, which held up to {newest}: {body}"
    );
}

#[then(regex = r"^the guider's metrics gain a frame with (\d+) usable stars$")]
async fn gain_usable_frame(world: &mut GuiderWorld, stars: u64) {
    let after = last_frame(world).await.unwrap_or(0);
    world
        .wait_for(
            "/api/v1/guiding/metrics",
            &format!("a frame after {after} with {stars} usable stars"),
            |status, body| {
                status == 200
                    && frames(body).iter().any(|frame| {
                        frame["frame"].as_u64().is_some_and(|number| number > after)
                            && frame["star_count"] == stars
                    })
            },
        )
        .await;
}

#[then(regex = r"^the guider's metrics gain at least (\d+) frames? after the camera came back$")]
async fn gain_after_comeback(world: &mut GuiderWorld, count: u64) {
    let after = last_frame(world).await.unwrap_or(0);
    let target = after + count;
    world
        .wait_for(
            "/api/v1/guiding/metrics",
            &format!("a frame numbered {target} or above"),
            |status, body| {
                status == 200
                    && frame_numbers(body)
                        .last()
                        .is_some_and(|last| *last >= target)
            },
        )
        .await;
}

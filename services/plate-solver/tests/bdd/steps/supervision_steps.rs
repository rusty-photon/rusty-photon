//! Step definitions for `subprocess_supervision.feature`.

use crate::world::{BackgroundAnswer, BackgroundRequest, PlateSolverWorld};
use cucumber::{then, when};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[when(expr = "I POST a (second )solve request with timeout {string} in the background")]
async fn when_post_solve_in_background(world: &mut PlateSolverWorld, timeout: String) {
    // `hold` mode's children record their start and wait for the release in
    // this directory. Must be set before the wrapper starts so it lands in
    // the wrapper's config.
    if world.hold_dir_path.is_none() {
        let dir = world.temp_dir_path().join("hold");
        std::fs::create_dir_all(&dir).expect("create hold dir");
        world.hold_dir_path = Some(dir);
    }
    if world.service_handle.is_none() {
        world.start_wrapper_with_mock().await;
    }
    let url = format!("{}/api/v1/solve", world.wrapper_url());
    let fits = world.fits_path.as_ref().expect("fits_path");
    let body = serde_json::json!({ "fits_path": fits, "timeout": timeout });
    world
        .background_requests
        .push(BackgroundRequest::spawn(url, body));
}

// A barrier on something the solve itself produced: once its child has
// recorded its start, its request holds the permit. The budget bounds a hang.
#[when("mock_astap has started that solve")]
async fn when_mock_astap_started_solve(world: &mut PlateSolverWorld) {
    let dir = hold_dir(world);
    let budget = Duration::from_secs(30);
    let started = Instant::now();
    while held_solves(&dir).is_empty() {
        assert!(
            started.elapsed() < budget,
            "no held mock_astap solve recorded its start in {} within {budget:?}",
            dir.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[when(expr = "{int} milliseconds pass")]
async fn when_milliseconds_pass(_world: &mut PlateSolverWorld, ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[then("the second request has not been answered")]
async fn then_second_request_unanswered(world: &mut PlateSolverWorld) {
    let second = request(world, "second");
    if second.is_answered() {
        let answer = second.answer().await;
        panic!(
            "the second request was answered while the first solve was still held: {:?}",
            answer.response
        );
    }
}

// The instant is taken before the file exists, so a floor measured from it is
// never short.
#[when("mock_astap is released")]
async fn when_mock_astap_released(world: &mut PlateSolverWorld) {
    let dir = hold_dir(world);
    world.released_at = Some(Instant::now());
    std::fs::File::create(dir.join("release")).expect("create release file");
}

#[then(expr = "the {word} request is answered with status {int}")]
async fn then_request_answered_with_status(
    world: &mut PlateSolverWorld,
    which: String,
    status: u16,
) {
    let answer: BackgroundAnswer = request(world, &which).answer().await.clone();
    assert_eq!(
        answer.response.status, status,
        "the {which} request: expected {status}, got {:?}",
        answer.response
    );
    world.last_response = Some(answer.response);
}

#[then(expr = "the second request was answered at least {int} milliseconds after the release")]
async fn then_second_answered_after_release(world: &mut PlateSolverWorld, ms: u64) {
    let released_at = world.released_at.expect("mock_astap was released");
    let answered_at = request(world, "second").answer().await.answered_at;
    let after = answered_at.saturating_duration_since(released_at);
    assert!(
        after >= Duration::from_millis(ms),
        "the second request was answered {after:?} after the release, under {ms}ms"
    );
}

#[then("mock_astap held only one solve")]
async fn then_mock_astap_held_one_solve(world: &mut PlateSolverWorld) {
    let dir = hold_dir(world);
    let held = held_solves(&dir);
    assert_eq!(
        held.len(),
        1,
        "expected one held solve in {}, got {held:?}",
        dir.display()
    );
}

fn hold_dir(world: &PlateSolverWorld) -> PathBuf {
    world
        .hold_dir_path
        .clone()
        .expect("hold_dir_path set by the background-request When step")
}

/// The `started-<pid>` files held `mock_astap` children recorded in `dir`.
fn held_solves(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read hold dir")
        .map(|entry| {
            entry
                .expect("hold dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.starts_with("started-"))
        .collect()
}

fn request<'a>(world: &'a mut PlateSolverWorld, which: &str) -> &'a mut BackgroundRequest {
    let index = match which {
        "first" => 0,
        "second" => 1,
        other => panic!("no {other:?} background request: expected first or second"),
    };
    world
        .background_requests
        .get_mut(index)
        .unwrap_or_else(|| panic!("no {which} background request was sent"))
}

// A floor on the response time, with no matching ceiling. The asymmetry is
// the point: load can only make a request take longer, so a floor states a
// real property of the wrapper (the grace period elapsed before the child was
// force-killed) that no amount of runner contention can falsify. A ceiling on
// the same measurement states nothing. The two escalation outcomes are 2s
// apart — less than the spread a contended 4-vCPU runner puts on a loopback
// round trip — so no threshold separates them, and a ceiling is evaluated only
// once the response has arrived, which is exactly the case where the wrapper
// did not hang. Which outcome occurred is read off the `message` field
// instead; the client timeout in `PlateSolverWorld::http_client` is what
// bounds a hang.
#[then(expr = "the response time is at least {int} milliseconds")]
async fn then_response_time_at_least(world: &mut PlateSolverWorld, ms: u64) {
    let elapsed = world.last_response_elapsed.expect("no elapsed");
    assert!(
        elapsed >= Duration::from_millis(ms),
        "elapsed {elapsed:?} < {ms}ms"
    );
}

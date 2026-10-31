//! Step definitions for `subprocess_supervision.feature`.

use crate::world::{BackgroundAnswer, BackgroundRequest, PlateSolverWorld};
use cucumber::{then, when};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The wrapper's debug lines the single-flight scenario reads, in
/// `services/plate-solver/src/api.rs`.
const QUEUED: &str = "solve request queued behind a running solve";
const STARTING: &str = "starting the solve";
const FINISHED: &str = "solve finished";

#[when(expr = "I POST a (second )solve request with timeout {string} in the background")]
async fn when_post_solve_in_background(world: &mut PlateSolverWorld, timeout: String) {
    // `hold` mode's children wait for the release in this directory. Must be
    // set before the wrapper starts so it lands in the wrapper's config.
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

// A barrier on the held child itself: it records its start only after
// finding no release, so a release from here on reaches a child that holds,
// and its request holds the permit until then.
#[when("mock_astap has started that solve")]
async fn when_mock_astap_started_solve(world: &mut PlateSolverWorld) {
    let dir = hold_dir(world);
    let budget = Duration::from_secs(30);
    let started = Instant::now();
    while !has_held_solve(&dir) {
        assert!(
            started.elapsed() < budget,
            "no held mock_astap solve recorded its start in {} within {budget:?}",
            dir.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// The wrapper logs the queueing only when the semaphore has refused the
// request, so this waits on the refusal itself. A second solve starting
// first means the semaphore let the request through.
#[then("the wrapper queues the second request behind the first solve")]
async fn then_wrapper_queues_second_request(world: &mut PlateSolverWorld) {
    wait_for_wrapper_log(world, "the second request queueing", |events| {
        let starts = events.iter().filter(|e| e.starts_with(STARTING)).count();
        assert!(
            starts < 2,
            "the wrapper started a second solve while the first held the permit: {events:?}"
        );
        events.contains(&QUEUED.to_owned()).then_some(())
    })
    .await;
}

// Here a wait that only gets longer under load: it keeps the first solve held,
// and so the second request queued, for at least this long after the wrapper
// logged the queueing. Nothing can answer the second request meanwhile.
#[when(expr = "{int} milliseconds pass")]
async fn when_milliseconds_pass(_world: &mut PlateSolverWorld, ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
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

// The wrapper writes these lines in the order it acts, and the second solve
// can only start once the first has finished and dropped the permit, so the
// order holds however slowly the runner runs. Both solves have finished by
// the time this runs; the wait only lets the forwarder catch up.
#[then(
    expr = "the wrapper started the second solve only after the first finished, with timeout {string}"
)]
async fn then_second_solve_started_after_first(world: &mut PlateSolverWorld, timeout: String) {
    let events = wait_for_wrapper_log(world, "both solves finishing", |events| {
        (events.iter().filter(|e| *e == FINISHED).count() >= 2).then(|| events.to_vec())
    })
    .await;
    let first_start = events
        .iter()
        .find(|e| e.starts_with(STARTING))
        .expect("the first solve's start")
        .clone();
    let expected = [
        first_start,
        QUEUED.to_owned(),
        FINISHED.to_owned(),
        format!("{STARTING} timeout={timeout}"),
        FINISHED.to_owned(),
    ];
    assert_eq!(
        events, expected,
        "the wrapper's single-flight events are out of order"
    );
}

/// Poll the wrapper's single-flight events — its [`QUEUED`], [`STARTING`]
/// and [`FINISHED`] lines, each from the message on, in the order logged —
/// until `done` returns `Some`. The budget bounds a hang.
async fn wait_for_wrapper_log<T>(
    world: &PlateSolverWorld,
    waiting_for: &str,
    done: impl Fn(&[String]) -> Option<T>,
) -> T {
    let handle = world.service_handle.as_ref().expect("wrapper started");
    let budget = Duration::from_secs(30);
    let started = Instant::now();
    loop {
        let events: Vec<String> = handle
            .stderr_lines()
            .iter()
            .filter_map(|line| {
                [QUEUED, STARTING, FINISHED]
                    .iter()
                    .find_map(|marker| line.find(marker))
                    .and_then(|at| line.get(at..))
                    .map(str::to_owned)
            })
            .collect();
        if let Some(found) = done(&events) {
            return found;
        }
        assert!(
            started.elapsed() < budget,
            "the wrapper did not log {waiting_for} within {budget:?}; its events: {events:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn hold_dir(world: &PlateSolverWorld) -> PathBuf {
    world
        .hold_dir_path
        .clone()
        .expect("hold_dir_path set by the background-request When step")
}

/// Whether a held `mock_astap` child has recorded its start in `dir`.
fn has_held_solve(dir: &Path) -> bool {
    std::fs::read_dir(dir).expect("read hold dir").any(|entry| {
        entry
            .expect("hold dir entry")
            .file_name()
            .to_string_lossy()
            .starts_with("started-")
    })
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

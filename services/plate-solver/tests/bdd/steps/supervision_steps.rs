//! Step definitions for `subprocess_supervision.feature`.

use crate::world::{ConcurrentResult, PlateSolverWorld};
use cucumber::{then, when};
use std::time::Duration;

#[when(expr = "I POST two concurrent solve requests with timeout {string} each")]
async fn when_two_concurrent_solves(world: &mut PlateSolverWorld, timeout: String) {
    // Each mock_astap child writes its spawn time to its own file under this
    // directory, so the Then step can observe serialization server-side. Must
    // be set before the wrapper starts so it lands in the wrapper's config.
    let spawn_dir = world.temp_dir_path().join("spawns");
    std::fs::create_dir_all(&spawn_dir).expect("create spawn dir");
    world.spawn_dir_path = Some(spawn_dir);

    // Make sure the wrapper is up before launching parallel requests.
    if world.service_handle.is_none() {
        world.start_wrapper_with_mock().await;
    }
    let url = format!("{}/api/v1/solve", world.wrapper_url());
    let fits = world.fits_path.as_ref().expect("fits_path").clone();
    let body = serde_json::json!({ "fits_path": fits, "timeout": timeout });

    let client = PlateSolverWorld::http_client();
    let make_request = || {
        let url = url.clone();
        let body = body.clone();
        let client = client.clone();
        async move {
            let resp = client.post(&url).json(&body).send().await.expect("POST");
            ConcurrentResult {
                status: resp.status().as_u16(),
            }
        }
    };

    let (a, b) = tokio::join!(make_request(), make_request());
    world.concurrent_results = vec![a, b];
}

#[then("both responses have status 504")]
async fn then_both_responses_504(world: &mut PlateSolverWorld) {
    assert_eq!(world.concurrent_results.len(), 2);
    for r in &world.concurrent_results {
        assert_eq!(r.status, 504, "expected both 504, got {r:?}");
    }
}

#[then("the two solves were serialized by the single-flight semaphore")]
async fn then_solves_serialized(world: &mut PlateSolverWorld) {
    // Observe serialization server-side: each `mock_astap` child writes its
    // spawn time (ns since the Unix epoch) to its own file under
    // `MOCK_ASTAP_SPAWN_DIR`. With a capacity-1 semaphore the second child
    // cannot spawn until the first releases the permit — which happens only
    // after the first request's full 100ms deadline elapses. So serialized
    // spawns are ~one deadline apart, while a parallel pair (if the semaphore
    // ever failed) would spawn near-simultaneously. The 50ms floor is half
    // the deadline: wide margin above runner jitter, yet a parallel
    // regression (gap ≈ 0) still trips it.
    //
    // This replaces an earlier check on client-side HTTP completion times
    // captured under `tokio::join!`. On a loaded Windows CI runner the test
    // task could be descheduled across both completions, collapsing the
    // observed gap below the threshold and failing even though serialization
    // worked. Spawn times are recorded inside the children, so they reflect
    // true server-side ordering regardless of how the client is scheduled.
    // Each child writes its own uniquely-named file; a shared append file
    // dropped writes across processes on Windows.
    let dir = world
        .spawn_dir_path
        .as_ref()
        .expect("spawn_dir_path set by the concurrent-request When step");
    let mut spawns: Vec<u128> = std::fs::read_dir(dir)
        .expect("read spawn dir")
        .map(|entry| {
            let path = entry.expect("spawn dir entry").path();
            std::fs::read_to_string(&path)
                .expect("read spawn file")
                .trim()
                .parse::<u128>()
                .expect("spawn timestamp parses as u128 ns")
        })
        .collect();
    assert_eq!(
        spawns.len(),
        2,
        "expected exactly two mock_astap spawn files in {}, got {spawns:?}",
        dir.display()
    );
    spawns.sort_unstable();
    let gap_ms = (spawns[1] - spawns[0]) / 1_000_000; // ns → ms
    assert!(
        gap_ms >= 50,
        "single-flight failed: child spawns only {gap_ms}ms apart (parallel?); \
         serialized spawns are ~one 100ms deadline apart (timestamps: {spawns:?})"
    );
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

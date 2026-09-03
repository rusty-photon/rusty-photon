//! The wall-clock budget frame-driven steps poll within, and the report
//! a timeout emits.
//!
//! Steps that wait for the engine to record frames are waiting on real
//! work — an `OmniSim` exposure, a sensor download, a blackboard persist,
//! and (when the scenario centres) a plate solve. How long that takes is
//! a property of the scenario, so the budget is derived from the
//! scenario's own parameters rather than fixed: a two-frame capture loop
//! and a four-frame centring run do not deserve the same allowance.
//!
//! The budget is a ceiling, not a target — a healthy run returns as soon
//! as the count lands, so widening it costs nothing until something is
//! already wrong. When it is spent, [`timeout_report`] answers the
//! question the bare count cannot: whether the pipeline was *slow* or
//! *stopped*. Those need opposite fixes, and a count alone cannot tell
//! them apart.

use std::fmt::Write as _;
use std::time::Duration;

use crate::world::SessionRunnerWorld;

/// Exposure assumed when a scenario pins no `exposure_duration`.
const DEFAULT_EXPOSURE: Duration = Duration::from_secs(2);

/// Per-frame wall clock beyond the exposure itself: sensor download,
/// blackboard persist, and the plate solve a centring scenario adds.
const PER_FRAME_OVERHEAD: Duration = Duration::from_secs(6);

/// Session start-up before the first frame can land — invocation,
/// equipment connect, and the initial slew.
const STARTUP_ALLOWANCE: Duration = Duration::from_secs(10);

/// Multiplier over the nominal estimate, sized for the slowest CI host
/// rather than the developer machine.
///
/// `windows-latest` has 4 vCPUs and runs several of this workspace's
/// 3-process (`OmniSim` + rp + session-runner) BDD trios concurrently,
/// where process creation and scheduling cost materially more than POSIX
/// `fork`/`exec`. Linux and macOS finish far inside this; the whole
/// point is that one number covers the worst platform so the budget does
/// not have to be tuned per host.
///
/// The delivered margin is the product of all three terms, not this
/// multiplier alone: the two-frame centring scenario derives 78s against
/// a measured ~9.9s on Linux, so ~7.9× — the nominal estimate is already
/// conservative before `SLACK` applies. Tune against that measurement
/// rather than against this constant.
const SLACK: u32 = 3;

/// How long a health probe waits before calling a service unresponsive.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The wall clock a step may wait for `frames` frames to be recorded.
///
/// Derived from the scenario's own `exposure_duration` when it pins one,
/// so a scenario that lengthens its exposures widens its budget without
/// anyone remembering to.
pub fn frame_budget(world: &SessionRunnerWorld, frames: u64) -> Duration {
    let per_frame = scenario_exposure(world).unwrap_or(DEFAULT_EXPOSURE) + PER_FRAME_OVERHEAD;
    let frames = u32::try_from(frames).unwrap_or(u32::MAX);
    (STARTUP_ALLOWANCE + per_frame.saturating_mul(frames)).saturating_mul(SLACK)
}

/// The `exposure_duration` the scenario staged, when it pinned one.
///
/// The parameter reaches the engine as the humantime string the feature
/// table wrote (`2s`); a bare number is accepted as seconds.
fn scenario_exposure(world: &SessionRunnerWorld) -> Option<Duration> {
    let raw = world
        .run_request
        .as_ref()?
        .get("params")?
        .get("exposure_duration")?;
    match raw.as_str() {
        Some(text) => humantime::parse_duration(text).ok(),
        None => raw.as_f64().map(Duration::from_secs_f64),
    }
}

/// Polls the persisted blackboard until `key` reaches `target`, then
/// returns; panics with [`timeout_report`] when the budget is spent.
pub async fn await_blackboard_counter(world: &SessionRunnerWorld, key: &str, target: u64) {
    let budget = frame_budget(world, target);
    let started = std::time::Instant::now();
    loop {
        let last = world.blackboard_counter(key).await;
        if last.is_some_and(|value| value >= target) {
            return;
        }
        let elapsed = started.elapsed();
        if elapsed >= budget {
            let headline = format!(
                "the blackboard never recorded {target} `{key}` within {budget:?} (last: {last:?})"
            );
            panic!(
                "{}",
                timeout_report(world, &headline, budget, elapsed).await
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A timeout report that distinguishes a starved pipeline from a stopped
/// one.
///
/// A service that answers `/health` while its counters sit still was
/// *slow* — the budget or the host is the problem. A service that stops
/// answering was *stopped*, and no budget would have saved the run.
pub async fn timeout_report(
    world: &SessionRunnerWorld,
    headline: &str,
    budget: Duration,
    elapsed: Duration,
) -> String {
    let mut report = String::new();
    let _ = writeln!(report, "{headline}");
    let _ = writeln!(report, "  waited {elapsed:?} of a {budget:?} budget");

    let _ = writeln!(report, "  rp /health: {}", probe(&world.rp_url()).await);
    match world.session_runner.as_ref() {
        Some(handle) => {
            let _ = writeln!(
                report,
                "  session-runner /health: {}",
                probe(&handle.base_url).await
            );
        }
        None => {
            let _ = writeln!(report, "  session-runner /health: not started");
        }
    }

    let _ = writeln!(report, "  blackboard: {}", blackboard_state(world).await);
    let _ = writeln!(report, "  events seen: {}", event_tally(world).await);
    report
}

/// `GET {base}/health`, reported as responsive, refused, or silent.
async fn probe(base_url: &str) -> String {
    let request = reqwest::Client::new()
        .get(format!("{base_url}/health"))
        .send();
    match tokio::time::timeout(PROBE_TIMEOUT, request).await {
        Ok(Ok(response)) => format!("responsive (HTTP {})", response.status().as_u16()),
        Ok(Err(error)) => format!("UNREACHABLE ({error})"),
        Err(_) => format!("SILENT (no answer within {PROBE_TIMEOUT:?})"),
    }
}

/// The persisted blackboard verbatim, so every counter is visible — not
/// only the one the step happened to poll.
///
/// Guards its own preconditions rather than calling
/// [`SessionRunnerWorld::blackboard_path`] blind: that panics when no
/// session has started, and a panic raised while building a failure
/// report would replace the diagnostic with a confusing second panic.
async fn blackboard_state(world: &SessionRunnerWorld) -> String {
    if world.state_dir.is_none() {
        return "no state dir (session-runner never started)".to_string();
    }
    if world.last_api_body.is_none() {
        return "no session id (no session was started)".to_string();
    }
    let path = world.blackboard_path();
    match tokio::fs::read_to_string(&path).await {
        Ok(contents) => format!("{} = {}", path.display(), contents.trim()),
        Err(error) => format!("{} unreadable ({error})", path.display()),
    }
}

/// Event types observed so far, with counts.
///
/// Separates "no exposure ever started" from "exposures start and never
/// complete" — the same distinction as the health probe, one layer up.
async fn event_tally(world: &SessionRunnerWorld) -> String {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    if let Some(client) = world.sse_client.as_ref() {
        for frame in client.frames().await {
            if let Some(event_type) = frame.event_type() {
                *counts.entry(event_type).or_default() += 1;
            }
        }
    }
    for event in world.received_events.read().await.iter() {
        *counts.entry(event.event_type.clone()).or_default() += 1;
    }
    if counts.is_empty() {
        return "none (no SSE client and no webhook deliveries)".to_string();
    }
    counts
        .into_iter()
        .map(|(event_type, count)| format!("{event_type}×{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

//! BDD test entry point for rp service

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::indexing_slicing
)]

#[path = "bdd/world.rs"]
mod world;

#[path = "bdd/steps/mod.rs"]
mod steps;

use std::sync::LazyLock;
use std::time::Instant;

/// Process-start reference for the `BDD-TRACE` breadcrumbs. Initialised on the
/// first [`trace`] call (≈ the first scenario), so the printed `+Ns` offsets
/// are relative-but-consistent — enough to read per-scenario durations
/// (`ENTER` vs `EXIT`) and the reset-vs-steps split (`ENTER` vs `reset-ok`)
/// straight from the log.
static PROCESS_START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Emit a one-line breadcrumb to stderr (bypassing cucumber's buffered writer),
/// tagged with elapsed seconds. The bdd leg runs with `--test_output=all`
/// (`.github/workflows/bazel.yml`), so these surface **even when `rp:bdd`
/// passes** — which is the failure mode we actually hit: the suite runs slow
/// (~36 min) but *completes*, so the time has to be read from a passing run.
/// `BDD-TRACE` makes the trail greppable; the `ENTER → reset-ok → EXIT` deltas
/// localise where the minutes go (per-scenario `OmniSim` reset vs the steps).
fn trace(msg: &str) {
    use std::io::Write as _;
    eprintln!(
        "BDD-TRACE +{:.1}s {msg}",
        PROCESS_START.elapsed().as_secs_f64()
    );
    let _ = std::io::stderr().flush();
}

bdd_infra::bdd_main! {
    use cucumber::World as _;
    use world::RpWorld;

    RpWorld::cucumber()
        .before(|feature, _rule, scenario, _world| {
            let scenario_name = scenario.name.clone();
            let feature_name = feature.name.clone();
            Box::pin(async move {
                trace(&format!(
                    "ENTER feature={feature_name:?} scenario={scenario_name:?}"
                ));
                // Reset every OmniSim device class our scenarios touch
                // (telescope, camera, filter wheel, focuser, cover
                // calibrator) to defaults before each scenario. The
                // shared OmniSim is a singleton across the BDD process,
                // so device state leaks between scenarios; the mount
                // leak that hung `park` in issue #143 is the case we
                // already hit. Each reset is a localhost PUT, run
                // sequentially (parallel resets raced OmniSim's
                // unsynchronised `AlpacaDevices` list — see
                // `reset_all_devices` for the writeup). We panic on
                // any reset failure that happens *after* the suite has
                // started its OmniSim — that's the loud-reset
                // diagnostic from #172. Failures from the very first
                // scenario's hook (before any Given step has called
                // `OmniSimHandle::start()`) are non-fatal:
                // connection-refused against the default port is the
                // expected case there and we don't want scenario 1 to
                // panic just because no stale OmniSim is around.
                if let Err(errors) =
                    bdd_infra::rp_harness::OmniSimHandle::reset_all_devices().await
                {
                    panic!("OmniSim device reset failed: {}", errors.join("; "));
                }
                trace(&format!("reset-ok scenario={scenario_name:?}"));
            })
        })
        .after(|_feature, _rule, scenario, _finished, maybe_world| {
            let scenario_name = scenario.name.clone();
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    // Abort background tool calls a failed scenario may not
                    // have joined — each holds its own MCP session whose
                    // streaming connection would block rp's graceful
                    // shutdown below, same as the persistent client.
                    for (_, handle) in world.background_calls.drain(..) {
                        handle.abort();
                    }
                    // Drop the MCP client and any SSE subscription first —
                    // their long-lived streaming HTTP connections would
                    // otherwise keep axum's graceful shutdown blocked, causing
                    // rp to time out and SIGKILL, which loses LLVM coverage
                    // profraw data (testing.md §5.4).
                    world.mcp_client = None;
                    world.sse_client = None;
                    if let Some(rp) = world.rp.as_mut() {
                        rp.stop().await;
                    }
                    // Stop the sky-survey-camera process, if any. Drop
                    // would handle this lazily, but doing it here keeps
                    // teardown deterministic for the @e2e-centering
                    // scenarios.
                    if let Some(cam) = world.sky_survey_camera.as_mut() {
                        cam.stop().await;
                    }
                }
                trace(&format!("EXIT scenario={scenario_name:?}"));
            })
        })
        .filter_run_and_exit("tests/features", |feat, _rule, sc| {
            let is_wip = feat.tags.iter().any(|t| t == "wip" || t == "@wip")
                || sc.tags.iter().any(|t| t == "wip" || t == "@wip");
            // Bazel sharding (BUILD `shard_count`): each shard process runs
            // only its deterministic slice of the scenarios, against its own
            // private OmniSim. Outside Bazel sharding this always passes.
            let in_shard = bdd_infra::sharding::scenario_in_current_shard(
                feat.path.as_deref(),
                &feat.name,
                sc.position.line,
            );
            !is_wip && in_shard
        })
        .await;

    // The cucumber suite has returned (all scenarios passed; `filter_run_and_exit`
    // returns rather than `process::exit`-ing on success — cucumber 0.22). Anything
    // that elapses on the rp:bdd action wall *after* this breadcrumb is teardown:
    // the `#[tokio::main]` runtime drop (which joins the blocking pool — a stuck
    // reqwest getaddrinfo here would park the process), then process exit +
    // `PR_SET_PDEATHSIG` reaping OmniSim. A post-scenario park here used to add
    // 20-37 min to the rp:bdd action wall; it was fixed by the CI park mitigations
    // (`--spawn_strategy=local` + `--remote_timeout=3`, now in `.bazelrc` — see
    // docs/plans/archive/bazel-migration.md). If this line prints but the action hangs
    // again, the hang is in/after the runtime drop, not in any scenario.
    trace("POST-RUN cucumber suite returned; entering tokio runtime drop (teardown)");
}

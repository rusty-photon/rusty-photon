//! `ConformU` compliance tests for the Star Adventurer `GTi` driver.
//!
//! Run the `ConformU` ASCOM Telescope test suite against the driver running
//! in mock mode. Same shape as the qhy-focuser integration test.
//!
//! The run is `ConformU`'s full test set: the runner drives the URL-argument
//! verbs, which call `SetFullTest()`, and it exposes no test selection. The
//! mount config's `site_latitude_deg` is device configuration — that is
//! honoured, and keeps the site above `ConformU`'s 10° gate so the
//! side-of-pier model tests are not skipped. Against the mock the run is
//! clean — no error, issue or configuration alert; the design doc's section
//! on running `ConformU` manually carries the expected report.
#![cfg(feature = "conformu")]
#![allow(clippy::await_holding_lock)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::unreachable)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::needless_pass_by_value,
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    clippy::used_underscore_binding,
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::suboptimal_flops,
    clippy::too_many_lines,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::float_cmp,
    clippy::similar_names,
    clippy::struct_excessive_bools
)]

use bdd_infra::{run_conformu, ConformuRun, ServiceHandle};
use std::sync::Mutex;
use tracing_subscriber::{fmt, EnvFilter};

static CONFORMU_LOCK: Mutex<()> = Mutex::new(());

/// What the service logs during the run when the caller has not set
/// `RUST_LOG` itself.
///
/// The RA East/West legs measure each pulse in wall time, so a restore
/// that reaches the wire late moves RA further and can fail a leg on a
/// host that stalled. The pulse lines at `debug` time both edges against
/// their schedule, and say whether a late restore waited on its timer,
/// on the axis lock, or on the wire. The crate's `debug` stays
/// event-driven, so the cost is about 500 lines a run, most of them
/// slew-watcher snapshots. A blanket `debug` would add `ascom-alpaca`'s
/// line for every request `ConformU` makes.
///
/// A non-empty `RUST_LOG` in the test's environment reaches the child
/// by inheritance and replaces this filter, even one meant for the test
/// harness, so it must keep `star_adventurer_gti=debug` for the edge
/// timings to show. An empty one counts as unset.
const SERVICE_LOG_FILTER: &str = "info,star_adventurer_gti=debug";

#[tokio::test]
async fn conformu_compliance_tests() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _lock = CONFORMU_LOCK.lock().unwrap();

    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_test_writer()
        .try_init();

    let test_dir = bdd_infra::scratch::new_dir("conformu-star-adventurer-gti-")?;

    let config_path = test_dir.path().join("config.json");

    // Mock-mode config: tagged USB transport with a placeholder port
    // (the binary is built with `--features mock`, so the
    // MockTransportFactory replaces both serial and UDP factories — the
    // device path doesn't matter).
    //
    // `polling_interval` is deliberately *not* a divisor of ConformU's
    // 5 s PulseGuide duration. The mock stops an axis instantly, so at
    // 200 ms (25 polls per pulse) the RA reads before and after a pulse
    // landed on the same poll phase, and any read that paired a stale
    // encoder sample with a live LST cancelled out — the mock could not
    // see the ±0.2 s cross-axis readings the hardware showed. 300 ms
    // leaves the two reads a third of a poll apart.
    //
    // `ra_pulse_edge_steps` is zero because the mock's motor board adds no
    // forward step at a rate change (its `rate_change_step_ticks` defaults
    // to 0); the shipped trim is a measurement of the real GTi and would
    // bend every East/West pulse against the mock by exactly the trim.
    let config = serde_json::json!({
        "transport": {
            "kind": "usb",
            "port": "/dev/mock",
            "baud_rate": 115_200,
            "command_timeout": "2s",
            "polling_interval": "300ms"
        },
        "server": {
            "port": 0
        },
        "mount": {
            "name": "ConformU Test Star Adventurer GTi",
            "unique_id": "conformu-star-adventurer-gti-001",
            "description": "Test mount for ConformU compliance",
            "enabled": true,
            "site_latitude_deg": 47.6062,
            "site_longitude_deg": -122.3321,
            "site_elevation_m": 56.0,
            "settle_after_slew": "200ms",
            "tracking_rate": "sidereal",
            "ra_pulse_edge_steps": { "east": 0.0, "west": 0.0 }
        }
    });

    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    let caller_set_rust_log = std::env::var_os("RUST_LOG").is_some_and(|v| !v.is_empty());
    let envs: &[(&str, &str)] = if caller_set_rust_log {
        &[]
    } else {
        &[("RUST_LOG", SERVICE_LOG_FILTER)]
    };
    let mut handle = ServiceHandle::try_start_with_env(
        env!("CARGO_PKG_NAME"),
        &[
            "--config",
            config_path
                .to_str()
                .expect("conformu temp path must be UTF-8"),
        ],
        envs,
    )
    .await?;

    println!("::group::ConformU Telescope Compliance Test Results");
    println!(
        "Running ASCOM Alpaca Telescope compliance tests on port {}...",
        handle.port
    );

    let result = run_conformu("telescope", &handle.base_url, 0, None).await;

    handle.stop().await;

    println!("::endgroup::");

    match result? {
        ConformuRun::Skipped => {
            println!("CONFORMU_PATH not set; skipped");
        }
        ConformuRun::Passed => {
            println!("ConformU compliance tests PASSED");
            println!("All ASCOM Alpaca Telescope compliance requirements met");
        }
    }

    Ok(())
}

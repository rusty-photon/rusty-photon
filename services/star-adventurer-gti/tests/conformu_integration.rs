//! `ConformU` compliance tests for the Star Adventurer `GTi` driver.
//!
//! Run the `ConformU` ASCOM Telescope test suite against the driver running
//! in mock mode. Same shape as the qhy-focuser integration test.
//!
//! The run is `ConformU`'s full test set: the runner drives the URL-argument
//! verbs, which call `SetFullTest()`, and it exposes no test selection. The
//! mount config's `site_latitude_deg` is device configuration — that is
//! honoured, and keeps the site above `ConformU`'s 10° gate so the
//! side-of-pier model tests are not skipped. Against the mock the run is not
//! green while the RA pulse-guide offset is open; the design doc's section
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
    // see the ±0.2 s cross-axis readings the hardware showed (issue
    // #1334). 300 ms leaves the two reads a third of a poll apart.
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
            "tracking_rate": "sidereal"
        }
    });

    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    let mut handle = ServiceHandle::try_start(
        env!("CARGO_PKG_NAME"),
        config_path
            .to_str()
            .expect("conformu temp path must be UTF-8"),
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

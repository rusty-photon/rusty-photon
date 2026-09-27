//! `ConformU` compliance tests for pa-falcon-rotator
//!
//! Verifies ASCOM Alpaca compliance for both the Rotator and the Status
//! Switch device by running the `ConformU` test suite against the driver
//! running in mock mode. Each test enables only the device it exercises so
//! the conformance run targets a single ASCOM class — mirroring the
//! ppba-driver split between its Switch and `ObservingConditions` tests.
// The std::Mutex is intentional here: it serializes sequential test runs because
// the ASCOM Alpaca discovery service binds to a fixed address.
#![cfg(feature = "conformu")]
#![allow(clippy::await_holding_lock)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
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

use bdd_infra::{FullRunSettings, ServiceHandle};
use std::sync::Mutex;
use tracing_subscriber::{fmt, EnvFilter};

// Static mutex ensures the two conformu tests run sequentially. Both bind
// the ASCOM Alpaca discovery service to its default port, so running them
// concurrently would race for that UDP socket regardless of the HTTP port.
static CONFORMU_LOCK: Mutex<()> = Mutex::new(());

#[tokio::test]
async fn conformu_compliance_tests_rotator() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = CONFORMU_LOCK.lock().unwrap();

    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_test_writer()
        .try_init();

    let test_dir = bdd_infra::scratch::new_dir("conformu-pa-falcon-rotator-")?;

    let config_path = test_dir.path().join("config.json");
    // ConformU's full test set runs (its URL verbs call SetFullTest()); the
    // settings shape only the run's timeouts. The mock rotator settles
    // instantly, so the default 60 s rotator timeout is longer than needed.
    let conformu_settings = FullRunSettings {
        rotator_timeout_s: 30,
        ..FullRunSettings::default()
    };

    let config = serde_json::json!({
        "serial": {
            "port": "/dev/mock",
            "baud_rate": 9600,
            "timeout": "2s"
        },
        "server": {
            "port": 0
        },
        "rotator": {
            "enabled": true,
            "name": "ConformU Test Falcon Rotator",
            "unique_id": "conformu-pa-falcon-rotator-001",
            "description": "Test Pegasus Falcon Rotator for ConformU compliance"
        },
        "switch": {
            "enabled": false,
            "name": "ConformU Test Falcon Status",
            "unique_id": "conformu-pa-falcon-rotator-status-001",
            "description": "Test Pegasus Falcon Status (disabled for rotator-only run)"
        }
    });

    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    // Pre-built pa-falcon-rotator binary must include the `mock` feature
    // (CI builds with --all-features); the binary is launched with the
    // mock serial port driving /dev/mock from the config.
    let mut handle = ServiceHandle::try_start(
        env!("CARGO_PKG_NAME"),
        config_path
            .to_str()
            .expect("conformu temp path must be UTF-8"),
    )
    .await
    .map_err(Box::<dyn std::error::Error>::from)?;

    println!("::group::ConformU Rotator Compliance Test Results");
    println!(
        "Running ASCOM Alpaca Rotator compliance tests on port {}...",
        handle.port
    );

    let result = bdd_infra::run_conformu("rotator", &handle.base_url, 0, Some(&conformu_settings))
        .await
        .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()));

    handle.stop().await;

    match result? {
        bdd_infra::ConformuRun::Skipped => {
            println!("CONFORMU_PATH not set; skipping rotator ConformU run");
        }
        bdd_infra::ConformuRun::Passed => {
            println!("ConformU Rotator compliance tests PASSED");
            println!("All ASCOM Alpaca Rotator compliance requirements met");
        }
    }

    println!("::endgroup::");

    Ok(())
}

#[tokio::test]
async fn conformu_compliance_tests_switch() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = CONFORMU_LOCK.lock().unwrap();

    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_test_writer()
        .try_init();

    let test_dir = bdd_infra::scratch::new_dir("conformu-pa-falcon-rotator-switch-")?;

    let config_path = test_dir.path().join("config.json");
    // Reduced switch delays so the run finishes in ~35 s instead of ~8 min
    // (ConformU's defaults are 500 ms per read and 3000 ms per write). The
    // test set is always the full one: ConformU's URL verbs call
    // SetFullTest(), which also force-enables the switch write tests.
    let conformu_settings = FullRunSettings {
        switch_read_delay_ms: 50,
        switch_write_delay_ms: 100,
        ..FullRunSettings::default()
    };

    let config = serde_json::json!({
        "serial": {
            "port": "/dev/mock",
            "baud_rate": 9600,
            "timeout": "2s"
        },
        "server": {
            "port": 0
        },
        "rotator": {
            "enabled": false,
            "name": "ConformU Test Falcon Rotator",
            "unique_id": "conformu-pa-falcon-rotator-001",
            "description": "Test Pegasus Falcon Rotator (disabled for switch-only run)"
        },
        "switch": {
            "enabled": true,
            "name": "ConformU Test Falcon Status",
            "unique_id": "conformu-pa-falcon-rotator-status-001",
            "description": "Test Pegasus Falcon Status sensors for ConformU compliance"
        }
    });

    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    let mut handle = ServiceHandle::try_start(
        env!("CARGO_PKG_NAME"),
        config_path
            .to_str()
            .expect("conformu temp path must be UTF-8"),
    )
    .await
    .map_err(Box::<dyn std::error::Error>::from)?;

    println!("::group::ConformU Switch Compliance Test Results");
    println!(
        "Running ASCOM Alpaca Switch compliance tests on port {}...",
        handle.port
    );

    let result = bdd_infra::run_conformu("switch", &handle.base_url, 0, Some(&conformu_settings))
        .await
        .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()));

    handle.stop().await;

    match result? {
        bdd_infra::ConformuRun::Skipped => {
            println!("CONFORMU_PATH not set; skipping switch ConformU run");
        }
        bdd_infra::ConformuRun::Passed => {
            println!("ConformU Switch compliance tests PASSED");
            println!("All ASCOM Alpaca Switch compliance requirements met");
        }
    }

    println!("::endgroup::");

    Ok(())
}

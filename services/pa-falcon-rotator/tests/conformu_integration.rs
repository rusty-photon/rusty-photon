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

use bdd_infra::ServiceHandle;
use std::sync::Mutex;
use tracing_subscriber::{fmt, EnvFilter};

// Static mutex ensures the two conformu tests run sequentially. Both bind
// the ASCOM Alpaca discovery service to its default port, so running them
// concurrently would race for that UDP socket regardless of the HTTP port.
static CONFORMU_LOCK: Mutex<()> = Mutex::new(());

/// Settings block shared by both tests. `ConformU` silently overwrites
/// partial settings files with defaults, so this carries the full template
/// produced by `echo '{}' > settings.json && conformu conformance
/// --settingsfile settings.json …` (see [`ConformUTestBuilder::settings_file`]
/// docs).
fn base_conformu_settings() -> serde_json::Value {
    serde_json::json!({
        "SettingsCompatibilityVersion": 1,
        "GoHomeOnDeviceSelected": true,
        "ConnectionTimeout": 2,
        "RunAs32Bit": false,
        "RiskAcknowledged": false,
        "DisplayMethodCalls": false,
        "UpdateCheck": false,
        "ApplicationPort": 0,
        "ConnectDisconnectTimeout": 5,
        "Debug": false,
        "TraceDiscovery": false,
        "TraceAlpacaCalls": false,
        "TestProperties": true,
        "TestMethods": true,
        "TestPerformance": false,
        "AlpacaDevice": {},
        "AlpacaConfiguration": {},
        "ComDevice": {},
        "ComConfiguration": {},
        "DeviceName": "No device selected",
        "DeviceTechnology": "NotSelected",
        "ReportGoodTimings": true,
        "ReportBadTimings": true,
        "TelescopeTests": {},
        "TelescopeExtendedRateOffsetTests": true,
        "TelescopeFirstUseTests": true,
        "TestSideOfPierRead": false,
        "TestSideOfPierWrite": false,
        "CameraFirstUseTests": true,
        "CameraTestImageArrayVariant": true,
    })
}

#[tokio::test]
async fn conformu_compliance_tests_rotator() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = CONFORMU_LOCK.lock().unwrap();

    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_test_writer()
        .try_init();

    let test_dir = std::env::temp_dir().join("conformu_pa_falcon_rotator_test");
    std::fs::create_dir_all(&test_dir)?;

    let config_path = test_dir.join("config.json");
    let conformu_settings_path = test_dir.join("conformu-settings.json");

    let mut conformu_settings = base_conformu_settings();
    // Mirror qhy-focuser's `FocuserTimeout: 30` precedent — the default
    // `RotatorTimeout: 60` is unnecessarily long for the mock backend.
    conformu_settings
        .as_object_mut()
        .expect("base settings must be a JSON object")
        .insert("RotatorTimeout".into(), serde_json::json!(30));
    std::fs::write(
        &conformu_settings_path,
        serde_json::to_string_pretty(&conformu_settings)?,
    )?;

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

    let result = bdd_infra::run_conformu(
        "rotator",
        &handle.base_url,
        0,
        Some(&conformu_settings_path),
    )
    .await
    .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()));

    handle.stop().await;
    std::fs::remove_dir_all(&test_dir).ok();

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

    let test_dir = std::env::temp_dir().join("conformu_pa_falcon_rotator_switch_test");
    std::fs::create_dir_all(&test_dir)?;

    let config_path = test_dir.join("config.json");
    let conformu_settings_path = test_dir.join("conformu-settings.json");

    let mut conformu_settings = base_conformu_settings();
    // Switch-specific overrides mirror ppba-driver: drop the SwitchReadDelay
    // / SwitchWriteDelay defaults (500ms / 3000ms) so the run finishes in
    // ~35s instead of ~8min for CI.
    {
        let obj = conformu_settings
            .as_object_mut()
            .expect("base settings must be a JSON object");
        obj.insert("SwitchEnableSet".into(), serde_json::json!(false));
        obj.insert("SwitchReadDelay".into(), serde_json::json!(50));
        obj.insert("SwitchWriteDelay".into(), serde_json::json!(100));
        obj.insert(
            "SwitchExtendedNumberTestRange".into(),
            serde_json::json!(100),
        );
        obj.insert("SwitchAsyncTimeout".into(), serde_json::json!(10));
        obj.insert("SwitchTestOffsets".into(), serde_json::json!(true));
    }
    std::fs::write(
        &conformu_settings_path,
        serde_json::to_string_pretty(&conformu_settings)?,
    )?;

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

    let result =
        bdd_infra::run_conformu("switch", &handle.base_url, 0, Some(&conformu_settings_path))
            .await
            .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()));

    handle.stop().await;
    std::fs::remove_dir_all(&test_dir).ok();

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

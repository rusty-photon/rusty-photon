//! `ConformU` compliance tests for PPBA driver
//!
//! These tests verify ASCOM Alpaca compliance by running the `ConformU` test suite
//! against the driver running in mock mode.
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

use bdd_infra::{run_conformu, ConformuRun, ServiceHandle};
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::test]
async fn conformu_compliance_tests() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize tracing to capture ConformU detailed output
    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_test_writer()
        .try_init();

    // Create test config
    let test_dir = std::env::temp_dir().join("conformu_ppba_test");
    std::fs::create_dir_all(&test_dir)?;

    let config_path = test_dir.join("config.json");
    let conformu_settings_path = test_dir.join("conformu-settings.json");

    // Create ConformU settings with reduced delays for faster CI
    // Note: ConformU requires a complete settings file - partial files are ignored
    let conformu_settings = serde_json::json!({
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
        // Switch-specific settings with reduced delays for faster CI
        // Default: SwitchReadDelay=500, SwitchWriteDelay=3000
        "SwitchEnableSet": false,
        "SwitchReadDelay": 50,
        "SwitchWriteDelay": 100,
        "SwitchExtendedNumberTestRange": 100,
        "SwitchAsyncTimeout": 10,
        "SwitchTestOffsets": true,
        // ObservingConditions-specific settings
        "ObservingConditionsNumReadings": 5,
        "ObservingConditionsReadInterval": 50
    });
    std::fs::write(
        &conformu_settings_path,
        serde_json::to_string_pretty(&conformu_settings)?,
    )?;

    let config = serde_json::json!({
        "serial": {
            "port": "/dev/mock",
            "baud_rate": 9600,
            "polling_interval": "60s",
            "timeout": "2s"
        },
        "server": {
            "port": 0
        },
        "switch": {
            "enabled": true,
            "name": "ConformU Test PPBA",
            "unique_id": "conformu-ppba-001",
            "description": "Test PPBA Switch for ConformU compliance"
        },
        "observingconditions": {
            "enabled": true,
            "name": "ConformU Test PPBA Weather",
            "unique_id": "conformu-ppba-weather-001",
            "description": "Test PPBA ObservingConditions for ConformU compliance"
        }
    });

    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    // Pre-built ppba-driver binary must include the `mock` feature
    // (CI builds with --all-features); the binary is launched with the
    // mock serial port driving /dev/mock from the config.
    let mut handle = ServiceHandle::try_start(
        env!("CARGO_PKG_NAME"),
        config_path
            .to_str()
            .expect("conformu temp path must be UTF-8"),
    )
    .await?;

    // Capture both run errors so `handle.stop()` below is unconditional and
    // the service gets a graceful SIGTERM with a chance to flush coverage data.
    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
        println!("::group::ConformU Switch Compliance Test Results");
        println!(
            "Running ASCOM Alpaca Switch compliance tests on port {}...",
            handle.port
        );

        match run_conformu("switch", &handle.base_url, 0, Some(&conformu_settings_path)).await? {
            ConformuRun::Skipped => {
                println!("ConformU Switch: CONFORMU_PATH not set, skipping.");
            }
            ConformuRun::Passed => {
                println!("ConformU Switch compliance tests PASSED");
            }
        }
        println!("::endgroup::");

        println!("::group::ConformU ObservingConditions Compliance Test Results");
        println!(
            "Running ASCOM Alpaca ObservingConditions compliance tests on port {}...",
            handle.port
        );

        match run_conformu(
            "observingconditions",
            &handle.base_url,
            0,
            Some(&conformu_settings_path),
        )
        .await?
        {
            ConformuRun::Skipped => {
                println!("ConformU ObservingConditions: CONFORMU_PATH not set, skipping.");
            }
            ConformuRun::Passed => {
                println!("ConformU ObservingConditions compliance tests PASSED");
            }
        }
        println!("::endgroup::");

        Ok(())
    }
    .await;

    handle.stop().await;
    std::fs::remove_dir_all(&test_dir).ok();

    result?;
    Ok(())
}

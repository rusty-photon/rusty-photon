//! `ConformU` compliance tests for UPBv2 driver
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
use tracing_subscriber::{fmt, EnvFilter};
use upbv2_driver::mock::ENV_AUTO_DEW;

/// Auto-dew mask putting channels A and B under device control and leaving C
/// free, so one run covers both sides of the per-channel `CanWrite` gate.
const AUTO_DEW_A_AND_B: &str = "5";

/// Startup deadline for the second pass, matching the 30 s `ServiceHandle::try_start`
/// applies to the first.
///
/// `start_with_env` is the only starter that takes child environment, and it
/// documents itself as having no deadline: a child that neither prints
/// `bound_addr=` nor exits blocks in it forever. Without this the two passes
/// of one test would fail differently — the first in 30 s, the second only
/// when the Bazel test timeout killed the whole target, with no clue which
/// pass wedged. The child is spawned `kill_on_drop`, so a timeout here takes
/// it down with the dropped future.
const START_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

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
    let test_dir = bdd_infra::scratch::new_dir("conformu-upbv2-driver-")?;

    let config_path = test_dir.path().join("config.json");
    let conformu_settings_path = test_dir.path().join("conformu-settings.json");

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
            "name": "ConformU Test UPBv2",
            "unique_id": "conformu-upbv2-001",
            "description": "Test UPBv2 Switch for ConformU compliance"
        },
        "observingconditions": {
            "enabled": true,
            "name": "ConformU Test UPBv2 Weather",
            "unique_id": "conformu-upbv2-weather-001",
            "description": "Test UPBv2 ObservingConditions for ConformU compliance"
        }
    });

    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    // Pre-built upbv2-driver binary must include the `mock` feature
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
    result?;

    // Second pass with auto-dew engaged. The default mock reports auto-dew
    // off, so every dew channel is writable and the gated half of the
    // per-channel `CanWrite` rule never runs. ASCOM requires a switch
    // reporting `CanWrite = false` to raise `MethodNotImplemented` from
    // SetSwitch/SetSwitchValue, and ConformU checks that pairing — so this is
    // the only run that proves the gate's error classification is compliant.
    let mut gated = tokio::time::timeout(
        START_DEADLINE,
        ServiceHandle::start_with_env(
            env!("CARGO_PKG_NAME"),
            &[
                "--config",
                config_path
                    .to_str()
                    .expect("conformu temp path must be UTF-8"),
            ],
            &[(ENV_AUTO_DEW, AUTO_DEW_A_AND_B)],
        ),
    )
    .await
    .map_err(|_| {
        format!("upbv2-driver did not bind within {START_DEADLINE:?} on the auto-dew pass")
    })?;

    let gated_result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
        println!("::group::ConformU Switch Compliance Test Results (auto-dew on A and B)");
        println!(
            "Running ASCOM Alpaca Switch compliance tests on port {}...",
            gated.port
        );

        match run_conformu("switch", &gated.base_url, 0, Some(&conformu_settings_path)).await? {
            ConformuRun::Skipped => {
                println!("ConformU Switch (auto-dew): CONFORMU_PATH not set, skipping.");
            }
            ConformuRun::Passed => {
                println!("ConformU Switch (auto-dew) compliance tests PASSED");
            }
        }
        println!("::endgroup::");

        Ok(())
    }
    .await;

    gated.stop().await;

    gated_result?;
    Ok(())
}

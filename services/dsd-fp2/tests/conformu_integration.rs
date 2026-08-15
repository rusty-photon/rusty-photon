//! `ConformU` compliance tests for the Deep Sky Dad FP2 driver.
//!
//! Spawns the dsd-fp2 binary with the mock transport factory and points
//! `ConformU` at it. The binary must be pre-built with `--features mock`
//! (which `--features conformu` implies); CI builds with
//! `--all-features` so the right path is exercised automatically.

#![cfg(feature = "conformu")]
#![allow(clippy::await_holding_lock)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use bdd_infra::ServiceHandle;
use std::sync::Mutex;
use tracing_subscriber::{fmt, EnvFilter};

static CONFORMU_LOCK: Mutex<()> = Mutex::new(());

#[tokio::test]
async fn conformu_compliance_tests() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = CONFORMU_LOCK.lock().unwrap();

    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_test_writer()
        .try_init();

    let test_dir = std::env::temp_dir().join("conformu_dsd_fp2_test");
    std::fs::create_dir_all(&test_dir)?;

    let config_path = test_dir.join("config.json");
    let conformu_settings_path = test_dir.join("conformu-settings.json");

    let conformu_settings = serde_json::json!({
        "SettingsCompatibilityVersion": 1,
        "GoHomeOnDeviceSelected": true,
        "ConnectionTimeout": 5,
        "RunAs32Bit": false,
        "RiskAcknowledged": false,
        "DisplayMethodCalls": false,
        "UpdateCheck": false,
        "ApplicationPort": 0,
        "ConnectDisconnectTimeout": 10,
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
        "FocuserTimeout": 30,
        "CoverCalibratorTimeout": 60
    });
    std::fs::write(
        &conformu_settings_path,
        serde_json::to_string_pretty(&conformu_settings)?,
    )?;

    let config = serde_json::json!({
        "serial": {
            "port": "/dev/mock",
            "baud_rate": 115_200,
            "polling_interval": "500ms",
            "timeout": "3s"
        },
        "server": {
            "port": 0
        },
        "cover_calibrator": {
            "enabled": true,
            "name": "ConformU Test DSD FP2",
            "unique_id": "conformu-dsd-fp2-001",
            "description": "Test Deep Sky Dad FP2 for ConformU compliance",
            "max_brightness": 4096
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

    println!("::group::ConformU CoverCalibrator Compliance Test Results");
    println!(
        "Running ASCOM Alpaca CoverCalibrator compliance tests on port {}...",
        handle.port
    );

    let result = bdd_infra::run_conformu(
        "covercalibrator",
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
            println!("CONFORMU_PATH not set; skipped");
        }
        bdd_infra::ConformuRun::Passed => {
            println!("ConformU compliance tests PASSED");
            println!("All ASCOM Alpaca CoverCalibrator compliance requirements met");
        }
    }

    println!("::endgroup::");

    Ok(())
}

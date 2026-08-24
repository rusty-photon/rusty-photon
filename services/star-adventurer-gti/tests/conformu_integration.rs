//! `ConformU` compliance tests for the Star Adventurer `GTi` driver.
//!
//! Run the `ConformU` ASCOM Telescope test suite against the driver running
//! in mock mode. Same shape as the qhy-focuser integration test.
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

    let test_dir = std::env::temp_dir().join("conformu_star_adventurer_gti_test");
    std::fs::create_dir_all(&test_dir)?;

    let config_path = test_dir.join("config.json");
    let conformu_settings_path = test_dir.join("conformu-settings.json");

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
        "TelescopeExtendedRateOffsetTests": false,
        "TelescopeFirstUseTests": false,
        "TestSideOfPierRead": true,
        "TestSideOfPierWrite": false,
        "CameraFirstUseTests": false,
        "CameraTestImageArrayVariant": false,
        "FocuserTimeout": 30
    });
    std::fs::write(
        &conformu_settings_path,
        serde_json::to_string_pretty(&conformu_settings)?,
    )?;

    // Mock-mode config: tagged USB transport with a placeholder port
    // (the binary is built with `--features mock`, so the
    // MockTransportFactory replaces both serial and UDP factories — the
    // device path doesn't matter).
    let config = serde_json::json!({
        "transport": {
            "kind": "usb",
            "port": "/dev/mock",
            "baud_rate": 115_200,
            "command_timeout": "2s",
            "polling_interval": "200ms"
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

    let result = run_conformu(
        "telescope",
        &handle.base_url,
        0,
        Some(&conformu_settings_path),
    )
    .await;

    handle.stop().await;
    std::fs::remove_dir_all(&test_dir).ok();

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

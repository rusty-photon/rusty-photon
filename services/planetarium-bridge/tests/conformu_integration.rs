//! `ConformU` compliance tests for the planetarium-bridge virtual Telescope.
//!
//! Runs the `ConformU` suites in their `*-settings` variants (the device under
//! test is configured inside the settings file): the URL-argument commands
//! force-enable every test via `ConformU`'s `SetFullTest()`, and this device's
//! capability set cannot satisfy the full set — with `CanPulseGuide = false`
//! the protocol suite's `PulseGuide` test polls `IsPulseGuiding` as its
//! completion check and records the spec-mandated `NOT_IMPLEMENTED` answer as
//! an error, so that one test is deselected via `TelescopeTests`.
//!
//! Two deliberate config choices (docs/services/planetarium-bridge.md
//! §ConformU): the reported-position altitude floor is `null` — the floor is
//! a client-UX policy, not device semantics, and would mask the sync
//! round-trip when `ConformU` syncs below it — and rp points at a dead
//! loopback port so the sync-fired imports land in the scenario's spool
//! instead of a live rp.
#![cfg(feature = "conformu")]
// The ConformU settings json! literal exceeds the default macro recursion
// limit once the full TelescopeTests dictionary is spelled out.
#![recursion_limit = "256"]
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
use bdd_infra::{run_conformu_from_settings, ConformuRun};
use std::sync::Mutex;
use tracing_subscriber::{fmt, EnvFilter};

static CONFORMU_LOCK: Mutex<()> = Mutex::new(());

#[tokio::test]
async fn conformu_compliance_tests() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _lock = CONFORMU_LOCK.lock().unwrap();

    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("ascom_alpaca::conformu=trace,info")),
        )
        .with_test_writer()
        .try_init();

    let test_dir = std::env::temp_dir().join("conformu_planetarium_bridge_test");
    std::fs::create_dir_all(&test_dir)?;

    let config_path = test_dir.join("config.json");
    let conformu_settings_path = test_dir.join("conformu-settings.json");
    let spool_path = test_dir.join("spool.jsonl");
    // ConformU's sync tests fire imports; a stale spool from a previous run
    // would make backlog assertions ambiguous.
    let _ = std::fs::remove_file(&spool_path);

    let config = serde_json::json!({
        "server": {
            "port": 0
        },
        "site": {
            "site_latitude_deg": 33.0,
            "site_longitude_deg": -117.0,
            "site_elevation_m": 0.0
        },
        // Port 1 on loopback is closed: every sync-fired import spools
        // (fast connection refusal) instead of reaching a live rp.
        "rp": {
            "mcp_server_url": "http://127.0.0.1:1/mcp"
        },
        "device": {
            "unique_id": "conformu-planetarium-bridge-001",
            // ConformU's AbortSlew test validates Slewing == true a fixed
            // 1.5 s after starting an async slew, so the convergence
            // window must comfortably exceed that.
            "slew_duration": "5s",
            "report_altitude_floor_deg": null
        },
        "spool": {
            "path": spool_path.to_str().expect("conformu temp path must be UTF-8")
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

    // The settings carry the device address (the `*-settings` commands read
    // it from here), so they are written after the service has a port.
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
        "DeviceTechnology": "Alpaca",
        "DeviceType": "Telescope",
        "DeviceName": "Planetarium Bridge",
        "AlpacaDevice": {
            "ServiceType": "Http",
            "ServerName": "127.0.0.1",
            "IpAddress": "127.0.0.1",
            "IpPort": handle.port,
            "InterfaceVersion": 1,
            "AscomDeviceType": "Telescope",
            "AlpacaDeviceNumber": 0,
            "AscomDeviceName": "Planetarium Bridge"
        },
        "AlpacaConfiguration": {},
        "ComDevice": {},
        "ComConfiguration": {},
        "ReportGoodTimings": true,
        "ReportBadTimings": true,
        // The full test dictionary (ConformU replaces a partial one with
        // all-enabled defaults). PulseGuide is deselected: with
        // CanPulseGuide = false the conformance suite requires
        // IsPulseGuiding to raise NOT_IMPLEMENTED, while the protocol
        // suite's PulseGuide test polls IsPulseGuiding as its completion
        // check and counts that same NOT_IMPLEMENTED as an error. The
        // test only exercises a verb the device does not have.
        "TelescopeTests": {
            "CanMoveAxis": true,
            "Park/Unpark": true,
            "AbortSlew": true,
            "AxisRate": true,
            "FindHome": true,
            "MoveAxis": true,
            "PulseGuide": false,
            "SlewToCoordinates": true,
            "SlewToCoordinatesAsync": true,
            "SlewToTarget": true,
            "SlewToTargetAsync": true,
            "DestinationSideOfPier": true,
            "SlewToAltAz": true,
            "SlewToAltAzAsync": true,
            "SyncToCoordinates": true,
            "SyncToTarget": true,
            "SyncToAltAz": true
        },
        "TelescopeExtendedRateOffsetTests": true,
        "TelescopeFirstUseTests": true,
        "TestSideOfPierRead": false,
        "TestSideOfPierWrite": false,
        "CameraFirstUseTests": true,
        "CameraTestImageArrayVariant": true,
        "FocuserTimeout": 30
    });
    std::fs::write(
        &conformu_settings_path,
        serde_json::to_string_pretty(&conformu_settings)?,
    )?;

    println!("::group::ConformU Telescope Compliance Test Results");
    println!(
        "Running ASCOM Alpaca Telescope compliance tests on port {}...",
        handle.port
    );

    // Capture both builder-construction and run-time errors so `handle.stop()`
    // below is unconditional and the service gets a graceful SIGTERM with a
    // chance to flush coverage data.
    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
        match run_conformu_from_settings(&conformu_settings_path).await? {
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
    .await;

    match &result {
        Ok(()) => {}
        Err(e) => {
            println!("ConformU compliance tests FAILED");
            println!("Error: {e}");
        }
    }
    println!("::endgroup::");

    handle.stop().await;
    result
}

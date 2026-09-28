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

use std::time::Duration;

use ascom_alpaca::api::{Switch, TypedDevice};
use ascom_alpaca::Client;
use bdd_infra::{run_conformu, ConformuRun, FullRunSettings, ServiceHandle};
use tracing_subscriber::{fmt, EnvFilter};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

const AUTO_DEW: usize = 5;
const DEW_HEATERS: [usize; 2] = [2, 3];

/// Discover the Switch device through the typed client, retrying for up to
/// ~2 s while the service's Alpaca routes come up.
async fn discover_switch(base_url: &str) -> Result<std::sync::Arc<dyn Switch>, String> {
    let client = Client::new(base_url).map_err(|e| e.to_string())?;
    for _ in 0..20 {
        if let Ok(devices) = client.get_devices().await {
            for device in devices {
                if let TypedDevice::Switch(switch) = device {
                    return Ok(switch);
                }
            }
            return Err(format!("no Switch device served at {base_url}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("Switch discovery at {base_url} failed 20 times"))
}

/// Turn auto-dew on through switch 5, as a client would, then check that both
/// dew heaters now report read-only. Without that check a mock that ignored
/// `PD` would let the auto-dew pass silently re-run the auto-dew-off case.
async fn engage_auto_dew(base_url: &str) -> TestResult {
    let switch = discover_switch(base_url).await?;
    switch.set_connected(true).await?;
    switch.set_switch_value(AUTO_DEW, 1.0).await?;
    for heater in DEW_HEATERS {
        if switch.can_write(heater).await? {
            return Err(format!("dew heater {heater} is still writable with auto-dew on").into());
        }
    }
    // ConformU connects for itself; the mock keeps auto-dew across the
    // disconnect, and the next connect's handshake reads it back.
    switch.set_connected(false).await?;
    Ok(())
}

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
    let test_dir = bdd_infra::scratch::new_dir("conformu-ppba-driver-")?;

    let config_path = test_dir.path().join("config.json");

    // Reduced switch delays so the run finishes in seconds rather than
    // minutes (ConformU's defaults are 500 ms per read and 3000 ms per
    // write). The test set is always the full one: ConformU's URL verbs
    // call SetFullTest(), which also force-enables the switch write tests.
    let conformu_settings = FullRunSettings {
        switch_read_delay_ms: 50,
        switch_write_delay_ms: 100,
        ..FullRunSettings::default()
    };

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

        match run_conformu("switch", &handle.base_url, 0, Some(&conformu_settings)).await? {
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
            Some(&conformu_settings),
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

    // Second Switch pass with auto-dew on, the state a PPBA normally runs in.
    // The mock starts with auto-dew off, so the pass above drives both dew
    // heaters through ConformU's write tests but never sees them read-only.
    // ASCOM requires a switch reporting `CanWrite = false` to raise
    // `MethodNotImplemented` from SetSwitch/SetSwitchValue, and ConformU
    // checks that pairing, so this is the run that holds the auto-dew
    // refusal to it. ConformU reads each switch's `CanWrite` before testing
    // it and walks the ids upward, so heaters 2 and 3 are judged before it
    // toggles auto-dew at switch 5.
    let mut gated = ServiceHandle::try_start(
        env!("CARGO_PKG_NAME"),
        config_path
            .to_str()
            .expect("conformu temp path must be UTF-8"),
    )
    .await?;

    let gated_result: TestResult = async {
        engage_auto_dew(&gated.base_url).await?;

        println!("::group::ConformU Switch Compliance Test Results (auto-dew on)");
        println!(
            "Running ASCOM Alpaca Switch compliance tests on port {}...",
            gated.port
        );

        match run_conformu("switch", &gated.base_url, 0, Some(&conformu_settings)).await? {
            ConformuRun::Skipped => {
                println!("ConformU Switch (auto-dew on): CONFORMU_PATH not set, skipping.");
            }
            ConformuRun::Passed => {
                println!("ConformU Switch (auto-dew on) compliance tests PASSED");
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

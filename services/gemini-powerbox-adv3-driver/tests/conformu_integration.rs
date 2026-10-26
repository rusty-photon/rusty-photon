//! `ConformU` compliance tests for the Advanced 3 driver.
//!
//! These tests verify ASCOM Alpaca compliance by running the `ConformU` test
//! suite against the driver running in mock mode: two Switch passes, one
//! with both dew channels in Manual (the hub's power-up state, where the
//! duty rows are writable and the enabled rows are not) and one with both
//! in Auto (the reverse), and both `ObservingConditions` suites once.
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

use bdd_infra::{run_conformu, ConformuRun, FullRunSettings, ServiceHandle};
use gemini_powerbox_adv3_driver::mock::ENV_DEW_MODES;
use tracing_subscriber::{fmt, EnvFilter};

/// Both dew channels in Auto: the enabled rows writable, the duty rows not.
const BOTH_AUTO: &str = "00";

#[tokio::test]
async fn conformu_compliance_tests() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_test_writer()
        .try_init();

    let test_dir = bdd_infra::scratch::new_dir("conformu-gemini-powerbox-adv3-driver-")?;
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
            "polling_interval": "60s",
            "timeout": "2s"
        },
        "server": {
            "port": 0
        },
        "switch": {
            "enabled": true,
            "name": "ConformU Test Gemini Advanced 3",
            "unique_id": "conformu-gemini-adv3-001",
            "description": "Test Advanced 3 Switch for ConformU compliance"
        },
        "observingconditions": {
            "enabled": true,
            "name": "ConformU Test Gemini Advanced 3 Weather",
            "unique_id": "conformu-gemini-adv3-weather-001",
            "description": "Test Advanced 3 ObservingConditions for ConformU compliance"
        }
    });
    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;
    let config_arg = config_path
        .to_str()
        .expect("conformu temp path must be UTF-8");

    // Pass one: the hub as it powers up, both channels in Manual.
    let mut handle = ServiceHandle::try_start(env!("CARGO_PKG_NAME"), config_arg).await?;

    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
        println!("::group::ConformU Switch Compliance Test Results (both channels Manual)");
        println!(
            "Running ASCOM Alpaca Switch compliance tests on port {}...",
            handle.port()
        );
        match run_conformu("switch", &handle.base_url(), 0, Some(&conformu_settings)).await? {
            ConformuRun::Skipped => println!("ConformU Switch: CONFORMU_PATH not set, skipping."),
            ConformuRun::Passed => println!("ConformU Switch compliance tests PASSED"),
        }
        println!("::endgroup::");

        println!("::group::ConformU ObservingConditions Compliance Test Results");
        println!(
            "Running ASCOM Alpaca ObservingConditions compliance tests on port {}...",
            handle.port()
        );
        match run_conformu(
            "observingconditions",
            &handle.base_url(),
            0,
            Some(&conformu_settings),
        )
        .await?
        {
            ConformuRun::Skipped => {
                println!("ConformU ObservingConditions: CONFORMU_PATH not set, skipping.");
            }
            ConformuRun::Passed => println!("ConformU ObservingConditions compliance tests PASSED"),
        }
        println!("::endgroup::");
        Ok(())
    }
    .await;

    handle.stop().await;
    result?;

    // Pass two: both channels in Auto. The duty rows now report `CanWrite =
    // false` and must raise `MethodNotImplemented` from SetSwitch /
    // SetSwitchValue, which ConformU checks — the only run that proves the
    // mode gate's error classification on that side.
    let mut gated = ServiceHandle::try_start_with_env(
        env!("CARGO_PKG_NAME"),
        &["--config", config_arg],
        &[(ENV_DEW_MODES, BOTH_AUTO)],
    )
    .await
    .map_err(|e| format!("auto pass: {e}"))?;

    let gated_result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
        println!("::group::ConformU Switch Compliance Test Results (both channels Auto)");
        println!(
            "Running ASCOM Alpaca Switch compliance tests on port {}...",
            gated.port()
        );
        match run_conformu("switch", &gated.base_url(), 0, Some(&conformu_settings)).await? {
            ConformuRun::Skipped => {
                println!("ConformU Switch (Auto): CONFORMU_PATH not set, skipping.");
            }
            ConformuRun::Passed => println!("ConformU Switch (Auto) compliance tests PASSED"),
        }
        println!("::endgroup::");
        Ok(())
    }
    .await;

    gated.stop().await;
    gated_result?;
    Ok(())
}
